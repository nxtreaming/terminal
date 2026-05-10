use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use browser_use_protocol::{
    failure_from_events, result_from_events, sanitized_agent_context_from_events, ModelEvent,
    SessionMeta, SessionStatus, ToolCall, ToolSpec,
};
use browser_use_providers::{ModelProvider, ProviderTurn, ScriptedProvider};
use browser_use_python_worker::{PythonWorker, PythonWorkerEvent, RunPythonResponse};
use browser_use_store::{AgentSummary, Store};
use serde_json::Value;

const MAX_TOOL_OUTPUT_TEXT_CHARS: usize = 16_000;

pub struct FakeAgentOptions<'a> {
    pub python_code: Option<&'a str>,
}

pub struct AgentRunOptions {
    pub max_turns: usize,
    pub max_context_chars: usize,
    pub browser_mode: Option<String>,
    pub python_tool_timeout_seconds: u64,
}

impl Default for AgentRunOptions {
    fn default() -> Self {
        Self {
            max_turns: 80,
            max_context_chars: 240_000,
            browser_mode: None,
            python_tool_timeout_seconds: 120,
        }
    }
}

impl AgentRunOptions {
    pub fn with_browser_mode(mut self, mode: impl Into<String>) -> Self {
        self.browser_mode = Some(mode.into());
        self
    }

    pub fn with_python_tool_timeout_seconds(mut self, timeout_seconds: u64) -> Self {
        self.python_tool_timeout_seconds = timeout_seconds;
        self
    }
}

pub fn run_fake_agent(
    store: &Store,
    task_text: &str,
    cwd: impl AsRef<Path>,
    options: FakeAgentOptions<'_>,
) -> Result<String> {
    let provider = if let Some(code) = options.python_code {
        ScriptedProvider::new(vec![
            vec![
                ModelEvent::TextDelta {
                    text: "Starting browser task.\n".to_string(),
                },
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "call_python".to_string(),
                        name: "python".to_string(),
                        arguments: serde_json::json!({ "code": code }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "call_done".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({
                            "result": "Python tool completed."
                        }),
                    },
                },
                ModelEvent::Done,
            ],
        ])
    } else {
        ScriptedProvider::new(vec![vec![
            ModelEvent::TextDelta {
                text: format!("Fake result for: {task_text}"),
            },
            ModelEvent::Done,
        ]])
    };
    run_agent_with_provider(store, &provider, task_text, cwd, AgentRunOptions::default())
}

pub fn run_agent_with_provider<P: ModelProvider>(
    store: &Store,
    provider: &P,
    task_text: &str,
    cwd: impl AsRef<Path>,
    options: AgentRunOptions,
) -> Result<String> {
    let session = store.create_session(None, cwd.as_ref())?;
    store.append_event(
        &session.id,
        "session.input",
        serde_json::json!({ "text": task_text }),
    )?;
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": task_text,
    })];
    run_loaded_session_with_provider(store, provider, session, messages, options)
}

pub fn run_existing_session_with_provider<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session_id: &str,
    options: AgentRunOptions,
) -> Result<String> {
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    let events = store.events_for_session(session_id)?;
    let messages = provider_messages_from_events(&events);
    run_loaded_session_with_provider(store, provider, session, messages, options)
}

fn run_loaded_session_with_provider<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session: SessionMeta,
    mut messages: Vec<Value>,
    options: AgentRunOptions,
) -> Result<String> {
    let run_id = store.record_run_started(&session.id, Some(std::process::id() as i64))?;
    let result = (|| -> Result<String> {
        let mut worker = PythonWorker::start_with_browser_mode(options.browser_mode.as_deref())?;
        store.append_event(
            &session.id,
            "session.status",
            serde_json::json!({ "status": "running" }),
        )?;
        store.append_event(
            &session.id,
            "model.config",
            serde_json::json!({
                "provider": provider.provider_name(),
                "model": provider.model_name(),
            }),
        )?;

        let mut deadline_warning_emitted = false;
        for turn_idx in 0..options.max_turns {
            ensure_not_cancelled(store, &session.id)?;
            maybe_emit_deadline_warning(
                store,
                &session.id,
                turn_idx,
                options.max_turns,
                &mut deadline_warning_emitted,
            )?;
            let mut assistant_text = String::new();
            let mut tool_calls = Vec::new();

            for event in provider.start_turn(ProviderTurn {
                messages: messages.clone(),
                tools: browser_tool_specs(),
            })? {
                match event {
                    ModelEvent::TextDelta { text } => {
                        store.append_event(
                            &session.id,
                            "model.delta",
                            serde_json::json!({ "text": text }),
                        )?;
                        assistant_text.push_str(&text);
                    }
                    ModelEvent::Usage { usage } => {
                        store.append_event(
                            &session.id,
                            "model.usage",
                            serde_json::to_value(usage)?,
                        )?;
                    }
                    ModelEvent::ToolCall { call } => {
                        store.append_event(
                            &session.id,
                            "model.tool_call",
                            serde_json::to_value(&call)?,
                        )?;
                        tool_calls.push(call);
                    }
                    ModelEvent::Done => {}
                }
            }
            ensure_not_cancelled(store, &session.id)?;

            if !assistant_text.is_empty() || !tool_calls.is_empty() {
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": assistant_text,
                    "tool_calls": tool_calls.iter().map(tool_call_message).collect::<Vec<_>>(),
                }));
            }
            maybe_compact_messages(store, &session.id, &mut messages, options.max_context_chars)?;

            if tool_calls.is_empty() {
                if !assistant_text.trim().is_empty() {
                    store.append_event(
                        &session.id,
                        "session.done",
                        serde_json::json!({ "result": assistant_text.trim_end() }),
                    )?;
                    return Ok(session.id.clone());
                }
                continue;
            }

            for call in tool_calls {
                ensure_not_cancelled(store, &session.id)?;
                let outcome = dispatch_tool_call(store, &session, &mut worker, &call, &options)?;
                messages.extend(outcome.messages);
                maybe_compact_messages(
                    store,
                    &session.id,
                    &mut messages,
                    options.max_context_chars,
                )?;
                if outcome.finished {
                    return Ok(session.id.clone());
                }
            }
        }

        store.append_event(
            &session.id,
            "session.failed",
            serde_json::json!({ "error": "agent exceeded maximum provider turns" }),
        )?;
        bail!("agent exceeded maximum provider turns");
    })();
    let run_status = if is_cancelled(store, &session.id)? {
        "cancelled"
    } else if result.is_ok() {
        "done"
    } else {
        "failed"
    };
    store.finish_run(&run_id, run_status)?;
    result
}

fn ensure_not_cancelled(store: &Store, session_id: &str) -> Result<()> {
    if is_cancelled(store, session_id)? {
        bail!("agent cancelled");
    }
    Ok(())
}

fn is_cancelled(store: &Store, session_id: &str) -> Result<bool> {
    Ok(store
        .load_session(session_id)?
        .is_some_and(|session| session.status == SessionStatus::Cancelled))
}

fn maybe_emit_deadline_warning(
    store: &Store,
    session_id: &str,
    turn_idx: usize,
    max_turns: usize,
    emitted: &mut bool,
) -> Result<()> {
    if *emitted || max_turns < 2 || turn_idx + 1 < max_turns {
        return Ok(());
    }
    *emitted = true;
    store.append_event(
        session_id,
        "session.deadline_warning",
        serde_json::json!({
            "reason": "agent turn budget is nearly exhausted",
            "max_turns": max_turns,
            "remaining_turns": max_turns.saturating_sub(turn_idx),
        }),
    )?;
    Ok(())
}

fn maybe_compact_messages(
    store: &Store,
    session_id: &str,
    messages: &mut Vec<Value>,
    max_context_chars: usize,
) -> Result<()> {
    if max_context_chars == 0 {
        return Ok(());
    }
    let before_chars = serde_json::to_string(messages)?.len();
    if before_chars <= max_context_chars {
        return Ok(());
    }
    store.append_event(
        session_id,
        "session.compaction_started",
        serde_json::json!({
            "message_count": messages.len(),
            "chars": before_chars,
            "max_chars": max_context_chars,
        }),
    )?;
    let result = (|| -> Result<()> {
        let events = store.events_for_session(session_id)?;
        let context = sanitized_agent_context_from_events(&events);
        let mut compacted = vec![serde_json::json!({
            "role": "system",
            "content": format!(
                "Compacted prior browser-agent context:\n{}",
                serde_json::to_string_pretty(&context)?
            ),
        })];
        if let Some(pending_call) = pending_assistant_tool_call(messages) {
            let max_recent_content_chars = (max_context_chars / 4).max(200);
            compacted.push(compact_recent_message(
                pending_call.clone(),
                max_recent_content_chars,
            ));
        }
        *messages = compacted;
        Ok(())
    })();
    if let Err(error) = result {
        store.append_event(
            session_id,
            "session.compaction_failed",
            serde_json::json!({ "error": error.to_string() }),
        )?;
        return Err(error);
    }
    store.append_event(
        session_id,
        "session.compacted",
        serde_json::json!({
            "message_count": messages.len(),
            "chars": serde_json::to_string(messages)?.len(),
        }),
    )?;
    Ok(())
}

fn pending_assistant_tool_call(messages: &[Value]) -> Option<&Value> {
    let message = messages.last()?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let has_tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    has_tool_calls.then_some(message)
}

fn compact_recent_message(mut message: Value, max_content_chars: usize) -> Value {
    let Some(object) = message.as_object_mut() else {
        return message;
    };
    if let Some(content) = object.get_mut("content") {
        compact_content_value(content, max_content_chars);
    }
    message
}

fn compact_content_value(value: &mut Value, max_content_chars: usize) {
    match value {
        Value::String(text) => {
            if text.chars().count() > max_content_chars {
                *text = truncate_for_context(text, max_content_chars);
            }
        }
        Value::Array(parts) => {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if text.chars().count() > max_content_chars {
                        part["text"] = Value::String(truncate_for_context(text, max_content_chars));
                    }
                }
            }
        }
        _ => {}
    }
}

fn truncate_for_context(text: &str, max_chars: usize) -> String {
    let keep = max_chars.saturating_sub(48).max(32);
    let mut out = text.chars().take(keep).collect::<String>();
    out.push_str("\n[truncated]");
    out
}

fn provider_messages_from_events(events: &[browser_use_protocol::EventRecord]) -> Vec<Value> {
    let mut messages = Vec::new();
    for event in events {
        match event.event_type.as_str() {
            "agent.context" => {
                if let Some(context) = event.payload.get("context") {
                    messages.push(serde_json::json!({
                        "role": "system",
                        "content": format!("Inherited compact context from parent session:\n{}", context),
                    }));
                }
            }
            "session.input" | "session.followup" => {
                if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": text,
                    }));
                }
            }
            _ => {}
        }
    }
    messages
}

struct ToolDispatchOutcome {
    finished: bool,
    messages: Vec<Value>,
}

fn browser_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "python".to_string(),
            description: concat!(
                "Run Python in the persistent browser session. Browser-harness helpers are already imported when available: ",
                "goto_url, page_info, js, fill_input, click_at_xy, type_text, press_key, scroll, wait_for_load, ",
                "wait_for_element, wait_for_network_idle, capture_screenshot, current_tab, list_tabs, switch_tab, ",
                "new_tab, cdp, drain_events, and http_get. Do not import Playwright, Selenium, or Pyppeteer. ",
                "Use copy_artifact and emit_image for files and screenshots."
            )
            .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python code to run in the persistent browser namespace."
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "done".to_string(),
            description: "Finish the browser task with a final user-facing result.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": {
                        "type": "string",
                        "description": "Final answer for the user."
                    }
                },
                "required": ["result"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "spawn_agent".to_string(),
            description: "Create a separate helper session for bounded background exploration."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The bounded task for the helper session."
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional stable task path, such as flight-search. Relative paths are stored under /root/."
                    },
                    "nickname": {
                        "type": "string",
                        "description": "Optional short display name."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional helper role label."
                    },
                    "fork_mode": {
                        "type": "string",
                        "enum": ["summary", "none", "all", "last_n"],
                        "description": "How much parent context to provide. summary is sanitized and compact."
                    },
                    "fork_turns": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of recent user/follow-up turns to include when fork_mode is last_n."
                    }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "wait_agent".to_string(),
            description: "Read the compact status and final result for a helper session."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_session_id": {
                        "type": "string",
                        "description": "The helper session id or canonical helper path returned by spawn_agent."
                    }
                },
                "required": ["child_session_id"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "send_message".to_string(),
            description: "Queue a message for a helper session without waking a new turn."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_session_id": {
                        "type": "string",
                        "description": "The helper session id or canonical helper path returned by spawn_agent."
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to queue for the helper."
                    }
                },
                "required": ["child_session_id", "message"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "followup_task".to_string(),
            description: "Queue a follow-up message for a helper session and wake its next turn."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_session_id": {
                        "type": "string",
                        "description": "The helper session id or canonical helper path returned by spawn_agent."
                    },
                    "message": {
                        "type": "string",
                        "description": "The follow-up instruction for the helper."
                    }
                },
                "required": ["child_session_id", "message"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "list_agents".to_string(),
            description: "List helper sessions spawned by this task.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path_prefix": {
                        "type": "string",
                        "description": "Optional canonical path prefix, such as /root/research."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "close_agent".to_string(),
            description: "Cancel and close a helper session that is no longer needed.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_session_id": {
                        "type": "string",
                        "description": "The helper session id or canonical helper path returned by spawn_agent."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Short reason for closing the helper."
                    }
                },
                "required": ["child_session_id"],
                "additionalProperties": false
            }),
        },
    ]
}

fn tool_call_message(call: &ToolCall) -> Value {
    serde_json::json!({
        "id": call.id,
        "name": call.name,
        "arguments": call.arguments,
    })
}

fn dispatch_tool_call(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    worker: &mut PythonWorker,
    call: &ToolCall,
    options: &AgentRunOptions,
) -> Result<ToolDispatchOutcome> {
    match call.name.as_str() {
        "done" => dispatch_done_tool(store, session, call),
        "python" => dispatch_python_tool(
            store,
            session,
            worker,
            call,
            options.python_tool_timeout_seconds,
        ),
        "spawn_agent" => dispatch_spawn_agent_tool(store, session, call),
        "wait_agent" => dispatch_wait_agent_tool(store, session, call),
        "send_message" => dispatch_agent_message_tool(store, session, call, false),
        "followup_task" => dispatch_agent_message_tool(store, session, call, true),
        "list_agents" => dispatch_list_agents_tool(store, session, call),
        "close_agent" => dispatch_close_agent_tool(store, session, call),
        _ => dispatch_unknown_tool(store, session, call),
    }
}

fn dispatch_done_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let result = call
        .arguments
        .get("result")
        .and_then(Value::as_str)
        .or_else(|| call.arguments.get("text").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "done",
            "arguments": call.arguments,
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({ "name": "done" }),
    )?;
    store.append_event(
        &session.id,
        "session.done",
        serde_json::json!({ "result": result }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: true,
        messages: Vec::new(),
    })
}

fn dispatch_python_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    worker: &mut PythonWorker,
    call: &ToolCall,
    timeout_seconds: u64,
) -> Result<ToolDispatchOutcome> {
    let code = call
        .arguments
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "python",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let mut stream_error = None;
    let response = worker.run_with_events_and_timeout(
        &session.id,
        &session.cwd,
        &session.artifact_root,
        code,
        Some(timeout_seconds as f64),
        |event| {
            if stream_error.is_none() {
                if let Err(err) = record_python_worker_event(store, &session.id, &event) {
                    stream_error = Some(err);
                }
            }
        },
    )?;
    if let Some(err) = stream_error {
        return Err(err);
    }
    record_python_response_final_event(store, &session.id, &response)?;
    if response.ok {
        store.append_event(
            &session.id,
            "tool.finished",
            serde_json::json!({
                "name": "python",
                "tool_call_id": call.id,
            }),
        )?;
    } else {
        store.append_event(
            &session.id,
            "tool.failed",
            serde_json::json!({
                "name": "python",
                "tool_call_id": call.id,
                "error": response.error,
            }),
        )?;
    }
    let messages = vec![serde_json::json!({
        "role": "tool",
        "tool_call_id": call.id,
        "name": "python",
        "content": python_tool_message_content_value(&response)?,
    })];
    Ok(ToolDispatchOutcome {
        finished: false,
        messages,
    })
}

fn dispatch_spawn_agent_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let message = call
        .arguments
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "spawn_agent requires message",
        );
    }
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "spawn_agent",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let parent_events = store.events_for_session(&session.id)?;
    let fork_mode = call
        .arguments
        .get("fork_mode")
        .and_then(Value::as_str)
        .unwrap_or("summary");
    let fork_turns = call
        .arguments
        .get("fork_turns")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let inherited_context = inherited_context_for_spawn(&parent_events, fork_mode, fork_turns);
    let agent_path = normalize_agent_path(call.arguments.get("path").and_then(Value::as_str));
    let child = store.create_child_session(
        &session.id,
        &session.cwd,
        agent_path.as_deref(),
        call.arguments.get("nickname").and_then(Value::as_str),
        call.arguments.get("role").and_then(Value::as_str),
    )?;
    store.append_event(
        &child.id,
        "agent.context",
        serde_json::json!({
            "from_session_id": session.id,
            "fork_mode": fork_mode,
            "fork_turns": fork_turns,
            "context": inherited_context,
        }),
    )?;
    store.append_event(
        &child.id,
        "session.input",
        serde_json::json!({ "text": message }),
    )?;
    store.append_event(
        &session.id,
        "agent.spawned",
        serde_json::json!({
            "child_session_id": child.id,
            "agent_path": agent_path.clone(),
            "nickname": call.arguments.get("nickname").and_then(Value::as_str),
            "role": call.arguments.get("role").and_then(Value::as_str),
            "fork_mode": fork_mode,
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "spawn_agent",
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            call,
            "spawn_agent",
            serde_json::json!({
                "child_session_id": child.id,
                "agent_path": agent_path,
                "status": "created",
            }),
        )?],
    })
}

fn inherited_context_for_spawn(
    parent_events: &[browser_use_protocol::EventRecord],
    fork_mode: &str,
    fork_turns: usize,
) -> Value {
    match fork_mode {
        "none" => serde_json::json!({ "mode": "none" }),
        "last_n" => {
            let mut context = sanitized_agent_context_from_events(parent_events);
            let mut recent_turns = parent_events
                .iter()
                .filter(|event| {
                    matches!(
                        event.event_type.as_str(),
                        "session.input" | "session.followup"
                    )
                })
                .rev()
                .take(fork_turns)
                .filter_map(|event| {
                    event
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| {
                            serde_json::json!({
                                "type": event.event_type,
                                "text": text,
                            })
                        })
                })
                .collect::<Vec<_>>();
            recent_turns.reverse();
            context["recent_turns"] = Value::Array(recent_turns);
            context["mode"] = Value::String("last_n".to_string());
            context
        }
        "all" => {
            let mut context = sanitized_agent_context_from_events(parent_events);
            context["mode"] = Value::String("all_sanitized".to_string());
            context
        }
        _ => {
            let mut context = sanitized_agent_context_from_events(parent_events);
            context["mode"] = Value::String("summary".to_string());
            context
        }
    }
}

fn normalize_agent_path(path: Option<&str>) -> Option<String> {
    path.and_then(|path| (!path.trim().is_empty()).then(|| canonical_agent_path(path)))
}

fn canonical_agent_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed == "root" {
        return "/root".to_string();
    }
    let segments = trimmed
        .split('/')
        .filter_map(|segment| {
            let normalized = segment
                .trim()
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                        ch.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_string();
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return "/root".to_string();
    }
    if segments.first().is_some_and(|segment| segment == "root") {
        format!("/{}", segments.join("/"))
    } else {
        format!("/root/{}", segments.join("/"))
    }
}

fn resolve_child_agent(
    store: &Store,
    parent_session_id: &str,
    reference: &str,
) -> Result<Option<AgentSummary>> {
    let agents = store.list_child_agents(parent_session_id)?;
    if let Some(agent) = agents
        .iter()
        .find(|agent| agent.child_session_id == reference)
        .cloned()
    {
        return Ok(Some(agent));
    }
    let canonical = canonical_agent_path(reference);
    Ok(agents.into_iter().find(|agent| {
        agent.agent_path.as_deref() == Some(reference)
            || agent.agent_path.as_deref() == Some(canonical.as_str())
    }))
}

fn dispatch_wait_agent_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let Some(child_ref) = call
        .arguments
        .get("child_session_id")
        .and_then(Value::as_str)
    else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "wait_agent requires child_session_id",
        );
    };
    let Some(child_summary) = resolve_child_agent(store, &session.id, child_ref)? else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "wait_agent can only inspect children of the current session",
        );
    };
    let child_session_id = child_summary.child_session_id.clone();
    let agent_path = child_summary.agent_path.clone();
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "wait_agent",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let child = store
        .load_session(&child_session_id)?
        .with_context(|| format!("unknown child session id: {child_session_id}"))?;
    let child_events = store.events_for_session(&child_session_id)?;
    let messages = store
        .messages_for_agent(&child_session_id)?
        .into_iter()
        .map(|message| {
            serde_json::json!({
                "id": message.id,
                "author_session_id": message.author_session_id,
                "content": message.content,
                "trigger_turn": message.trigger_turn,
                "created_ms": message.created_ms,
            })
        })
        .collect::<Vec<_>>();
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "wait_agent",
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            call,
            "wait_agent",
            serde_json::json!({
                "child_session_id": child_session_id,
                "agent_path": agent_path,
                "status": child.status.as_str(),
                "result": result_from_events(&child_events),
                "failure": failure_from_events(&child_events),
                "messages": messages,
            }),
        )?],
    })
}

fn dispatch_agent_message_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    trigger_turn: bool,
) -> Result<ToolDispatchOutcome> {
    let tool_name = if trigger_turn {
        "followup_task"
    } else {
        "send_message"
    };
    let Some(child_ref) = call
        .arguments
        .get("child_session_id")
        .and_then(Value::as_str)
    else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            &format!("{tool_name} requires child_session_id"),
        );
    };
    let Some(child_summary) = resolve_child_agent(store, &session.id, child_ref)? else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            &format!("{tool_name} can only message children of the current session"),
        );
    };
    let child_session_id = child_summary.child_session_id.clone();
    let agent_path = child_summary.agent_path.clone();
    let message = call
        .arguments
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            &format!("{tool_name} requires message"),
        );
    }
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": tool_name,
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let child = store
        .load_session(&child_session_id)?
        .with_context(|| format!("unknown child session id: {child_session_id}"))?;
    debug_assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
    let mail = store.send_agent_message(&session.id, &child_session_id, message, trigger_turn)?;
    if trigger_turn {
        store.append_event(
            &child_session_id,
            "session.followup",
            serde_json::json!({ "text": message }),
        )?;
    }
    store.append_event(
        &session.id,
        "agent.message",
        serde_json::json!({
            "id": mail.id,
            "child_session_id": child_session_id,
            "content": message,
            "trigger_turn": trigger_turn,
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": tool_name,
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            call,
            tool_name,
            serde_json::json!({
                "message_id": mail.id,
                "child_session_id": child_session_id,
                "agent_path": agent_path,
                "trigger_turn": trigger_turn,
            }),
        )?],
    })
}

fn dispatch_list_agents_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "list_agents",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let path_prefix = call
        .arguments
        .get("path_prefix")
        .and_then(Value::as_str)
        .map(canonical_agent_path);
    let agents = store
        .list_child_agents(&session.id)?
        .into_iter()
        .filter(|agent| match path_prefix.as_deref() {
            Some(prefix) => agent
                .agent_path
                .as_deref()
                .is_some_and(|path| path == prefix || path.starts_with(&format!("{prefix}/"))),
            None => true,
        })
        .map(|agent| {
            serde_json::json!({
                "child_session_id": agent.child_session_id,
                "status": agent.status,
                "path": agent.agent_path,
                "nickname": agent.agent_nickname,
                "role": agent.agent_role,
                "updated_ms": agent.updated_ms,
            })
        })
        .collect::<Vec<_>>();
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "list_agents",
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            call,
            "list_agents",
            serde_json::json!({ "agents": agents }),
        )?],
    })
}

fn dispatch_close_agent_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let Some(child_ref) = call
        .arguments
        .get("child_session_id")
        .and_then(Value::as_str)
    else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "close_agent requires child_session_id",
        );
    };
    let Some(child_summary) = resolve_child_agent(store, &session.id, child_ref)? else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "close_agent can only close children of the current session",
        );
    };
    let child_session_id = child_summary.child_session_id.clone();
    let agent_path = child_summary.agent_path.clone();
    let reason = call
        .arguments
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("closed by parent agent");
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "close_agent",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let child = store
        .load_session(&child_session_id)?
        .with_context(|| format!("unknown child session id: {child_session_id}"))?;
    debug_assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
    store.close_child_agent(&child_session_id, reason)?;
    store.append_event(
        &session.id,
        "agent.cancelled",
        serde_json::json!({
            "child_session_id": child_session_id,
            "status": "cancelled",
            "payload": { "reason": reason },
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "close_agent",
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            call,
            "close_agent",
            serde_json::json!({
                "child_session_id": child_session_id,
                "agent_path": agent_path,
                "status": "cancelled",
            }),
        )?],
    })
}

fn dispatch_tool_validation_error(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    error: &str,
) -> Result<ToolDispatchOutcome> {
    store.append_event(
        &session.id,
        "tool.failed",
        serde_json::json!({
            "name": call.name,
            "tool_call_id": call.id,
            "error": error,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![serde_json::json!({
            "role": "tool",
            "tool_call_id": call.id,
            "name": call.name,
            "content": error,
        })],
    })
}

fn dispatch_unknown_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let error = format!("unknown tool: {}", call.name);
    store.append_event(
        &session.id,
        "tool.failed",
        serde_json::json!({
            "name": call.name,
            "tool_call_id": call.id,
            "error": error,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![serde_json::json!({
            "role": "tool",
            "tool_call_id": call.id,
            "name": call.name,
            "content": error,
        })],
    })
}

fn tool_json_message(call: &ToolCall, name: &str, content: Value) -> Result<Value> {
    Ok(serde_json::json!({
        "role": "tool",
        "tool_call_id": call.id,
        "name": name,
        "content": serde_json::to_string(&content)?,
    }))
}

fn python_tool_message_content(response: &RunPythonResponse) -> String {
    if response.ok {
        let mut parts = Vec::new();
        if !response.text.trim().is_empty() {
            let text = response.text.trim();
            if text.chars().count() > MAX_TOOL_OUTPUT_TEXT_CHARS {
                parts.push(truncate_for_context(text, MAX_TOOL_OUTPUT_TEXT_CHARS));
            } else {
                parts.push(text.to_string());
            }
        }
        if !response.data.is_null() {
            parts.push(format!("data: {}", response.data));
        }
        if parts.is_empty() {
            "python tool completed".to_string()
        } else {
            parts.join("\n")
        }
    } else {
        format!(
            "python tool failed: {}",
            response
                .error
                .as_deref()
                .unwrap_or("unknown python worker error")
        )
    }
}

fn python_tool_message_content_value(response: &RunPythonResponse) -> Result<Value> {
    let text = python_tool_message_content(response);
    let Some(image_parts) = python_tool_image_output_parts(response)? else {
        return Ok(Value::String(text));
    };
    let mut parts = vec![serde_json::json!({
        "type": "output_text",
        "text": text,
    })];
    parts.extend(image_parts);
    Ok(Value::Array(parts))
}

fn python_tool_image_output_parts(response: &RunPythonResponse) -> Result<Option<Vec<Value>>> {
    if !response.ok || response.images.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for image in &response.images {
        let Some(path) = image.get("path").and_then(Value::as_str) else {
            continue;
        };
        let bytes = std::fs::read(path).with_context(|| format!("read image artifact {path}"))?;
        let mime_type = image
            .get("mime_type")
            .and_then(Value::as_str)
            .or_else(|| image.get("mime").and_then(Value::as_str))
            .unwrap_or("image/png");
        parts.push(serde_json::json!({
            "type": "input_image",
            "image_url": format!("data:{mime_type};base64,{}", general_purpose::STANDARD.encode(bytes)),
            "detail": image
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("auto"),
        }));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts))
}

pub fn record_python_response_events(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
) -> Result<()> {
    record_python_response_events_inner(store, session_id, response, true)
}

pub fn record_python_response_final_event(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
) -> Result<()> {
    record_python_response_events_inner(store, session_id, response, false)
}

pub fn record_python_worker_event(
    store: &Store,
    session_id: &str,
    event: &PythonWorkerEvent,
) -> Result<()> {
    match event.event.as_str() {
        "output" => record_python_output(store, session_id, &event.payload),
        "browser" => record_python_browser_event(store, session_id, &event.payload),
        "image" => record_python_image(store, session_id, &event.payload),
        "artifact" => record_python_artifact(store, session_id, &event.payload),
        _ => Ok(()),
    }
}

fn record_python_response_events_inner(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
    include_host_records: bool,
) -> Result<()> {
    if include_host_records {
        for output in &response.outputs {
            record_python_output(store, session_id, output)?;
        }

        for browser_event in &response.browser_events {
            record_python_browser_event(store, session_id, browser_event)?;
        }

        let image_paths = response
            .images
            .iter()
            .filter_map(|image| image.get("path").and_then(Value::as_str))
            .collect::<std::collections::HashSet<_>>();
        for image in &response.images {
            record_python_image(store, session_id, image)?;
        }

        for artifact in &response.artifacts {
            let Some(path) = artifact.get("path").and_then(Value::as_str) else {
                continue;
            };
            if image_paths.contains(path) {
                continue;
            }
            record_python_artifact(store, session_id, artifact)?;
        }
    }

    let (text, text_artifact) = spill_large_text_output(store, session_id, &response.text)?;
    let mut payload = serde_json::json!({
        "name": "python",
        "ok": response.ok,
        "text": text,
        "data": response.data,
        "images": response.images,
        "artifacts": response.artifacts,
        "browser_harness_available": response.browser_harness_available,
        "browser_harness_error": response.browser_harness_error,
    });
    if let Some(artifact) = text_artifact.as_ref() {
        payload["text_truncated"] = Value::Bool(true);
        payload["text_artifact"] = artifact.clone();
    }
    let event = store.append_event(session_id, "tool.output", payload)?;
    if let Some(artifact) = text_artifact {
        if let Some(path) = artifact
            .get("path")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        {
            store.record_artifact(
                session_id,
                Some(event.seq),
                "tool-output",
                &path,
                Some("text/plain"),
                artifact,
            )?;
        }
    }
    Ok(())
}

fn spill_large_text_output(
    store: &Store,
    session_id: &str,
    text: &str,
) -> Result<(String, Option<Value>)> {
    if text.chars().count() <= MAX_TOOL_OUTPUT_TEXT_CHARS {
        return Ok((text.to_string(), None));
    }
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    let output_dir = Path::new(&session.artifact_root).join("tool-output");
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let path = output_dir.join(format!("python-output-{}.txt", browser_use_store::now_ms()));
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok((
        truncate_for_context(text, MAX_TOOL_OUTPUT_TEXT_CHARS),
        Some(serde_json::json!({
            "kind": "tool-output",
            "path": path.display().to_string(),
            "mime": "text/plain",
            "bytes": std::fs::metadata(&path).ok().and_then(|metadata| i64::try_from(metadata.len()).ok()),
        })),
    ))
}

fn record_python_output(store: &Store, session_id: &str, output: &Value) -> Result<()> {
    store.append_event(
        session_id,
        "tool.output",
        serde_json::json!({
            "name": "python",
            "stream": true,
            "text": output.get("text").and_then(Value::as_str).unwrap_or_default(),
        }),
    )?;
    Ok(())
}

fn record_python_browser_event(
    store: &Store,
    session_id: &str,
    browser_event: &Value,
) -> Result<()> {
    if let Some(event_type) = browser_event.get("type").and_then(Value::as_str) {
        let payload = browser_event
            .get("payload")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        store.append_event(session_id, event_type, payload)?;
    }
    Ok(())
}

fn record_python_image(store: &Store, session_id: &str, image: &Value) -> Result<()> {
    let event = store.append_event(
        session_id,
        "tool.image",
        serde_json::json!({
            "name": "python",
            "image": image,
        }),
    )?;
    if let Some(path) = image.get("path").and_then(Value::as_str) {
        store.record_artifact(
            session_id,
            Some(event.seq),
            "image",
            path,
            image.get("mime_type").and_then(Value::as_str),
            image.clone(),
        )?;
    }
    Ok(())
}

fn record_python_artifact(store: &Store, session_id: &str, artifact: &Value) -> Result<()> {
    let Some(path) = artifact.get("path").and_then(Value::as_str) else {
        return Ok(());
    };
    let kind = artifact
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("file");
    let event = store.append_event(
        session_id,
        "artifact.created",
        serde_json::json!({
            "name": "python",
            "artifact": artifact,
        }),
    )?;
    store.record_artifact(
        session_id,
        Some(event.seq),
        kind,
        path,
        artifact.get("mime").and_then(Value::as_str),
        artifact.clone(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_agent_can_run_python_and_finish() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session_id = run_fake_agent(
            &store,
            "check python",
            temp.path(),
            FakeAgentOptions {
                python_code: Some("print('hello from python')\nresult = {'ok': True}"),
            },
        )?;
        let session = store.load_session(&session_id)?.expect("session");
        assert_eq!(session.status.as_str(), "done");
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "tool.started"));
        assert!(events.iter().any(
            |event| event.event_type == "model.tool_call" && event.payload["name"] == "python"
        ));
        assert!(events.iter().any(|event| event.event_type == "tool.output"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "model.tool_call" && event.payload["name"] == "done"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.done"));
        Ok(())
    }

    #[test]
    fn provider_done_tool_finishes_session() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![vec![
            ModelEvent::ToolCall {
                call: ToolCall {
                    id: "done_1".to_string(),
                    name: "done".to_string(),
                    arguments: serde_json::json!({"result": "final answer"}),
                },
            },
            ModelEvent::Done,
        ]]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "finish directly",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| event.event_type == "model.config"
            && event.payload["provider"] == "scripted"
            && event.payload["model"] == "scripted"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "tool.finished" && event.payload["name"] == "done"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.done"
                && event.payload["result"] == "final answer"));
        let runs = store.runs_for_session(&session_id)?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "done");
        assert!(runs[0].ended_ms.is_some());
        Ok(())
    }

    #[test]
    fn provider_loop_respects_external_cancel_before_finalizing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "cancel me"}),
        )?;
        let provider = CancellingProvider {
            state_dir: temp.path().to_path_buf(),
            session_id: session.id.clone(),
        };

        let result = run_existing_session_with_provider(
            &store,
            &provider,
            &session.id,
            AgentRunOptions::default(),
        );

        assert!(result.is_err());
        let session = store.load_session(&session.id)?.context("session")?;
        assert_eq!(session.status, SessionStatus::Cancelled);
        let events = store.events_for_session(&session.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.cancelled"));
        assert!(!events
            .iter()
            .any(|event| event.event_type == "session.done"));
        let runs = store.runs_for_session(&session.id)?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "cancelled");
        Ok(())
    }

    struct CancellingProvider {
        state_dir: std::path::PathBuf,
        session_id: String,
    }

    impl ModelProvider for CancellingProvider {
        fn provider_name(&self) -> &'static str {
            "cancelling"
        }

        fn model_name(&self) -> &str {
            "cancelling"
        }

        fn start_turn(&self, _turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            Store::open(&self.state_dir)?.request_cancel(&self.session_id, "test cancel")?;
            Ok(vec![
                ModelEvent::TextDelta {
                    text: "this should not become the final answer".to_string(),
                },
                ModelEvent::Done,
            ])
        }
    }

    #[test]
    fn provider_loop_records_python_tool_timeout_and_continues() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "python_timeout".to_string(),
                        name: "python".to_string(),
                        arguments: serde_json::json!({
                            "code": "import time\ntime.sleep(5)",
                        }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_timeout".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "timeout recovered"}),
                    },
                },
                ModelEvent::Done,
            ],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "timeout then recover",
            temp.path(),
            AgentRunOptions::default().with_python_tool_timeout_seconds(1),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "tool.failed"
                && event.payload["name"] == "python"
                && event.payload["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("timed out"))
        }));
        assert!(events.iter().any(|event| {
            event.event_type == "session.done" && event.payload["result"] == "timeout recovered"
        }));
        Ok(())
    }

    #[test]
    fn provider_messages_are_compacted_when_context_gets_large() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![
                ModelEvent::TextDelta {
                    text: "x".repeat(900),
                },
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "python_small".to_string(),
                        name: "python".to_string(),
                        arguments: serde_json::json!({"code": "result = {'ok': True}"}),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_compact".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "compacted ok"}),
                    },
                },
                ModelEvent::Done,
            ],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "compact this",
            temp.path(),
            AgentRunOptions {
                max_turns: 4,
                max_context_chars: 500,
                browser_mode: None,
                python_tool_timeout_seconds: 120,
            },
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.compaction_started"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.compacted"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.done"
                && event.payload["result"] == "compacted ok"));
        Ok(())
    }

    #[test]
    fn compaction_keeps_only_pending_tool_call_after_summary() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({ "text": "compact with pending call" }),
        )?;
        let mut messages = vec![
            serde_json::json!({
                "role": "user",
                "content": "x".repeat(2000),
            }),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "old_call",
                    "name": "python",
                    "arguments": { "code": "print('old')" },
                }],
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "old_call",
                "content": "old output",
            }),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "pending_call",
                    "name": "python",
                    "arguments": { "code": "print('pending')" },
                }],
            }),
        ];

        maybe_compact_messages(&store, &session.id, &mut messages, 500)?;

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "pending_call");
        assert!(!messages
            .iter()
            .any(|message| message.get("role").and_then(Value::as_str) == Some("tool")));
        Ok(())
    }

    #[test]
    fn provider_loop_emits_deadline_warning_on_final_turn() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![ModelEvent::Done],
            vec![ModelEvent::ToolCall {
                call: ToolCall {
                    id: "done_final".to_string(),
                    name: "done".to_string(),
                    arguments: serde_json::json!({"result": "finished on final turn"}),
                },
            }],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "deadline warning",
            temp.path(),
            AgentRunOptions {
                max_turns: 2,
                max_context_chars: 80_000,
                browser_mode: None,
                python_tool_timeout_seconds: 120,
            },
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.deadline_warning"
                && event.payload["remaining_turns"] == 1
                && event.payload["max_turns"] == 2));
        assert!(events.iter().any(|event| event.event_type == "session.done"
            && event.payload["result"] == "finished on final turn"));
        Ok(())
    }

    #[test]
    fn provider_can_use_agent_delegation_tools_without_copying_child_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = AgentToolProvider::default();
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "research flights",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let children = store.list_child_agents(&session_id)?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].status, "closed");
        assert_eq!(
            children[0].agent_path.as_deref(),
            Some("/root/flight-search")
        );

        let parent_events = store.events_for_session(&session_id)?;
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.spawned"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.cancelled"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.message"
                && event.payload["trigger_turn"] == false));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.message"
                && event.payload["trigger_turn"] == true));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "spawn_agent"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "send_message"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "followup_task"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "wait_agent"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "list_agents"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "close_agent"));

        let child_events = store.events_for_session(&children[0].child_session_id)?;
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "agent.context"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "agent.context"
                && event.payload["fork_mode"] == "summary"
                && event.payload["context"]["mode"] == "summary"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "session.input"
                && event.payload["text"] == "inspect flight search constraints"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "session.followup"
                && event.payload["text"] == "run the focused follow-up"));
        let mailbox = store.messages_for_agent(&children[0].child_session_id)?;
        assert_eq!(mailbox.len(), 2);
        assert!(!mailbox[0].trigger_turn);
        assert!(mailbox[1].trigger_turn);
        Ok(())
    }

    #[test]
    fn spawn_context_supports_none_and_last_n_modes() {
        let events = vec![
            browser_use_protocol::EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: serde_json::json!({"text": "first"}),
            },
            browser_use_protocol::EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s".to_string(),
                ts_ms: 2,
                event_type: "session.followup".to_string(),
                payload: serde_json::json!({"text": "second"}),
            },
            browser_use_protocol::EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s".to_string(),
                ts_ms: 3,
                event_type: "session.followup".to_string(),
                payload: serde_json::json!({"text": "third"}),
            },
            browser_use_protocol::EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "s".to_string(),
                ts_ms: 4,
                event_type: "tool.output".to_string(),
                payload: serde_json::json!({"text": "raw output"}),
            },
        ];
        let none = inherited_context_for_spawn(&events, "none", 0);
        assert_eq!(none["mode"], "none");
        assert!(none.get("recent_turns").is_none());

        let last = inherited_context_for_spawn(&events, "last_n", 2);
        assert_eq!(last["mode"], "last_n");
        assert_eq!(last["recent_turns"].as_array().unwrap().len(), 2);
        assert_eq!(last["recent_turns"][0]["text"], "second");
        assert_eq!(last["recent_turns"][1]["text"], "third");
        assert!(!last.to_string().contains("raw output"));
    }

    #[test]
    fn existing_session_provider_run_uses_sanitized_agent_context() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, temp.path())?;
        store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({ "text": "parent task" }),
        )?;
        store.append_event(
            &parent.id,
            "tool.output",
            serde_json::json!({ "name": "python", "text": "raw output that must stay out" }),
        )?;
        let parent_events = store.events_for_session(&parent.id)?;
        let context = sanitized_agent_context_from_events(&parent_events);
        let child =
            store.create_child_session(&parent.id, temp.path(), Some("child"), None, None)?;
        store.append_event(
            &child.id,
            "agent.context",
            serde_json::json!({
                "from_session_id": parent.id,
                "context": context,
            }),
        )?;
        store.append_event(
            &child.id,
            "session.input",
            serde_json::json!({ "text": "child task" }),
        )?;
        let provider = ContextAssertingProvider;
        let session_id = run_existing_session_with_provider(
            &store,
            &provider,
            &child.id,
            AgentRunOptions::default(),
        )?;
        assert_eq!(session_id, child.id);
        let events = store.events_for_session(&child.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.done"
                && event.payload["result"] == "context ok"));
        Ok(())
    }

    #[test]
    fn python_image_outputs_are_forwarded_to_next_model_turn() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("shot.png"), b"not-really-a-png")?;
        let store = Store::open(temp.path().join("state"))?;
        let provider = ImageInspectingProvider::default();
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "look at page",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| event.event_type == "tool.image"));
        assert!(events.iter().any(|event| event.event_type == "session.done"
            && event.payload["result"] == "image context ok"));
        Ok(())
    }

    #[derive(Default)]
    struct ImageInspectingProvider {
        step: std::sync::Mutex<usize>,
    }

    impl ModelProvider for ImageInspectingProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let mut step = self.step.lock().expect("step lock");
            let event = if *step == 0 {
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "python_image".to_string(),
                        name: "python".to_string(),
                        arguments: serde_json::json!({
                            "code": "emit_image('shot.png', label='page')",
                        }),
                    },
                }
            } else {
                assert!(turn.messages.iter().any(message_has_input_image));
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_image".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({
                            "result": "image context ok",
                        }),
                    },
                }
            };
            *step += 1;
            Ok(vec![event, ModelEvent::Done])
        }
    }

    fn message_has_input_image(message: &Value) -> bool {
        message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|part| {
                part.get("type").and_then(Value::as_str) == Some("input_image")
                    && part
                        .get("image_url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| url.starts_with("data:image/png;base64,"))
            })
    }

    struct ContextAssertingProvider;

    impl ModelProvider for ContextAssertingProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let joined = turn
                .messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(joined.contains("Inherited compact context"));
            assert!(joined.contains("parent task"));
            assert!(joined.contains("child task"));
            assert!(!joined.contains("raw output that must stay out"));
            Ok(vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_existing".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "context ok"}),
                    },
                },
                ModelEvent::Done,
            ])
        }
    }

    #[derive(Default)]
    struct AgentToolProvider {
        step: std::sync::Mutex<usize>,
    }

    impl ModelProvider for AgentToolProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let mut step = self.step.lock().expect("step lock");
            let event = match *step {
                0 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "spawn_1".to_string(),
                        name: "spawn_agent".to_string(),
                        arguments: serde_json::json!({
                            "message": "inspect flight search constraints",
                            "path": "flight-search",
                            "nickname": "Flight helper",
                            "role": "researcher",
                        }),
                    },
                },
                1 => {
                    assert_eq!(
                        agent_path_from_tool_messages(&turn).as_deref(),
                        Some("/root/flight-search")
                    );
                    ModelEvent::ToolCall {
                        call: ToolCall {
                            id: "send_1".to_string(),
                            name: "send_message".to_string(),
                            arguments: serde_json::json!({
                                "child_session_id": "/root/flight-search",
                                "message": "keep this in your notes",
                            }),
                        },
                    }
                }
                2 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "followup_1".to_string(),
                        name: "followup_task".to_string(),
                        arguments: serde_json::json!({
                            "child_session_id": "flight-search",
                            "message": "run the focused follow-up",
                        }),
                    },
                },
                3 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "wait_1".to_string(),
                        name: "wait_agent".to_string(),
                        arguments: serde_json::json!({
                            "child_session_id": "/root/flight-search",
                        }),
                    },
                },
                4 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "list_1".to_string(),
                        name: "list_agents".to_string(),
                        arguments: serde_json::json!({"path_prefix": "/root"}),
                    },
                },
                5 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "close_1".to_string(),
                        name: "close_agent".to_string(),
                        arguments: serde_json::json!({
                            "child_session_id": "flight-search",
                            "reason": "parent has enough context",
                        }),
                    },
                },
                _ => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_1".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({
                            "result": "delegation path verified",
                        }),
                    },
                },
            };
            *step += 1;
            Ok(vec![event, ModelEvent::Done])
        }
    }

    fn agent_path_from_tool_messages(turn: &ProviderTurn) -> Option<String> {
        turn.messages.iter().rev().find_map(|message| {
            if message.get("name").and_then(Value::as_str) != Some("spawn_agent") {
                return None;
            }
            let content = message.get("content").and_then(Value::as_str)?;
            serde_json::from_str::<Value>(content)
                .ok()?
                .get("agent_path")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
    }

    #[test]
    fn fake_agent_records_python_host_outputs_and_artifacts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input.txt");
        std::fs::write(&input, "hello")?;
        let store = Store::open(temp.path().join("state"))?;
        let session_id = run_fake_agent(
            &store,
            "copy artifact",
            temp.path(),
            FakeAgentOptions {
                python_code: Some(
                    "emit_output('copied')\ncopy_artifact('input.txt')\nemit_browser_live_url('https://live.example')\nprint('done')",
                ),
            },
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "artifact.created"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "browser.live_url"));
        let artifacts = store.artifacts_for_session(&session_id)?;
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].bytes, Some(5));
        Ok(())
    }

    #[test]
    fn large_python_output_spills_to_artifact() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path().join("state"))?;
        let session_id = run_fake_agent(
            &store,
            "spill output",
            temp.path(),
            FakeAgentOptions {
                python_code: Some("print('x' * 20000)"),
            },
        )?;
        let events = store.events_for_session(&session_id)?;
        let output = events
            .iter()
            .rev()
            .find(|event| {
                event.event_type == "tool.output"
                    && event.payload.get("stream").is_none()
                    && event.payload["name"] == "python"
            })
            .context("final python output event")?;
        assert_eq!(output.payload["text_truncated"], true);
        let path = output.payload["text_artifact"]["path"]
            .as_str()
            .context("text artifact path")?;
        assert!(std::path::Path::new(path).exists());
        assert!(output.payload["text"]
            .as_str()
            .unwrap_or_default()
            .contains("[truncated]"));
        let artifacts = store.artifacts_for_session(&session_id)?;
        assert!(artifacts
            .iter()
            .any(|artifact| artifact.kind == "tool-output"));
        Ok(())
    }
}
