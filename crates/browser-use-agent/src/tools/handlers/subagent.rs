//! Model-callable subagent orchestration tools.
//!
//! These are thin handlers over the durable Store-backed agent tree when a live
//! session store is available, falling back to the in-memory [`SubagentManager`]
//! for isolated/no-store runs. The manager still owns spawn, role application,
//! depth enforcement, the fallback live-agent registry, the EVENT-NOTIFY
//! mailbox, and the [`ChildSpawner`](crate::subagents::ChildSpawner) seam.
//!
//! Parity:
//! - tool names + arg shapes: codex `multi_agents_spec.rs` (`spawn_agent`,
//!   `wait_agent`, `send_input`, `send_message`, `followup_task`, `list_agents`,
//!   `close_agent`).
//! - lifecycle: a spawn enqueues a child through the manager and emits
//!   `subagent.spawned`; Store-backed messaging persists `agent.message` rows
//!   and mailbox entries; close updates the durable child edge where available.
//!
//! Each handler implements the [`ToolRuntime`] stack ONCE (like `done`): no
//! sandbox, no approval, never denied — they route through the orchestrator on
//! the SAME typed dispatch path as every other tool, returning the operation's
//! JSON result as the tool output `stdout`.

use std::sync::Arc;

use async_trait::async_trait;
use browser_use_store::{Store, StoreNotificationWatcher};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use tokio::time::Duration;
use tokio::time::Instant;

use crate::config_overrides::{
    ChildAgentCompletionHandler, ChildAgentRunCompletion, ChildAgentRunRequest, ChildAgentRunner,
    DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS, DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS,
    DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS,
};
use crate::context::typed_user_input_preview_from_items;
use crate::events::{EventSink, PendingEvent};
use crate::session::SharedStore;
use crate::subagents::mailbox::AgentStatus;
use crate::subagents::manager::{ParentContext, SubagentManager};
use crate::subagents::spawn::{check_spawn_depth, SpawnAgentArgs};
use crate::subagents::{
    cleanup_agent_runtime_state_for_agent_subtree, display_agent_path_for_session,
    final_statuses_for_v1_wait, last_task_message_for_agent, local_agent_status_value,
    resolve_agent_path_v2, resolve_agent_reference_in_tree_v2, session_was_interrupted,
    store_collect_agent_tree, store_resolve_agent_reference_in_tree,
    store_resolve_agent_reference_in_tree_v2, store_root_session_id,
};
use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::SandboxPreference;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitAgentTimeoutOptions {
    pub default_timeout_ms: i64,
    pub min_timeout_ms: i64,
    pub max_timeout_ms: i64,
}

impl Default for WaitAgentTimeoutOptions {
    fn default() -> Self {
        Self {
            default_timeout_ms: DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS,
            min_timeout_ms: DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS,
            max_timeout_ms: DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS,
        }
    }
}

/// Shared dependencies every subagent tool carries: the manager (lifecycle
/// owner) + the parent's context (for spawn) + a durable event sink (+ session
/// id) so lifecycle transitions are persisted for the TUI render.
///
/// Cloning is cheap (`Arc`s + a small `ParentContext`); each tool holds its own
/// clone so they share one manager/registry/mailbox.
#[derive(Clone)]
pub struct SubagentToolDeps {
    /// The lifecycle owner. Spawn/wait/send/list all route through it.
    pub manager: Arc<SubagentManager>,
    /// The parent's canonical path/depth/base-config, threaded into every
    /// `spawn` so depth limits + sticky provider/tier are enforced.
    pub parent: ParentContext,
    /// Durable + UI event sink. `subagent.*` events are emitted here so the
    /// TUI's existing subagent render sees lifecycle transitions.
    pub sink: Arc<dyn EventSink>,
    /// The session the events are scoped to (the parent's session id).
    pub session_id: String,
    /// Optional durable store context. Production CLI/TUI runs provide this so
    /// inter-agent messages, listing, waiting, and close operations work across
    /// turns and process-local dispatcher instances. Unit/no-store paths fall
    /// back to the in-memory manager.
    pub store: Option<SharedStore>,
    /// Optional child runner used to wake an existing child when `followup_task`
    /// targets an idle Store-backed agent.
    pub child_runner: Option<ChildAgentRunner>,
    /// Serializes the durable store limit check with the in-memory manager
    /// reservation. Store-backed child creation is synchronous before the runner
    /// returns, so the next spawn sees the just-created child in the store.
    pub spawn_gate: Arc<tokio::sync::Mutex<()>>,
    pub wait_timeouts: WaitAgentTimeoutOptions,
    pub hide_spawn_agent_metadata: bool,
    pub max_concurrent_threads_per_session: Option<usize>,
}

impl SubagentToolDeps {
    fn emit(&self, event_type: &str, payload: serde_json::Value) {
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            event_type,
            payload,
        ));
    }
}

fn now_ms() -> i64 {
    browser_use_store::now_ms()
}

fn default_reasoning_effort() -> String {
    "medium".to_string()
}

fn requested_model(model: Option<&String>) -> String {
    model.cloned().unwrap_or_default()
}

fn requested_reasoning_effort(reasoning_effort: Option<&String>) -> String {
    reasoning_effort
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(default_reasoning_effort)
}

fn effective_spawn_model(deps: &SubagentToolDeps, model: Option<&String>) -> String {
    model
        .cloned()
        .unwrap_or_else(|| deps.parent.base_config.model.clone())
}

fn effective_spawn_reasoning_effort(
    deps: &SubagentToolDeps,
    reasoning_effort: Option<&String>,
) -> String {
    reasoning_effort
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| deps.parent.base_config.reasoning_effort.clone())
        .unwrap_or_else(default_reasoning_effort)
}

fn insert_optional_string(
    map: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<String>,
) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value));
    }
}

fn collab_agent_ref(
    thread_id: impl Into<String>,
    nickname: Option<String>,
    role: Option<String>,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("thread_id".to_string(), Value::String(thread_id.into()));
    insert_optional_string(&mut map, "agent_nickname", nickname);
    insert_optional_string(&mut map, "agent_role", role);
    Value::Object(map)
}

fn collab_agent_status_entry(
    thread_id: impl Into<String>,
    nickname: Option<String>,
    role: Option<String>,
    status: Value,
) -> Value {
    let mut map = match collab_agent_ref(thread_id, nickname, role) {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    map.insert("status".to_string(), status);
    Value::Object(map)
}

fn emit_collab_spawn_begin(
    deps: &SubagentToolDeps,
    ctx: &ToolCtx,
    prompt: &str,
    model: String,
    reasoning_effort: String,
) {
    deps.emit(
        "collab_agent_spawn_begin",
        json!({
            "call_id": ctx.call_id,
            "started_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "prompt": prompt,
            "model": model,
            "reasoning_effort": reasoning_effort,
        }),
    );
}

struct CollabSpawnEnd<'a> {
    call_id: &'a str,
    new_thread_id: Option<String>,
    new_agent_nickname: Option<String>,
    new_agent_role: Option<String>,
    prompt: &'a str,
    model: String,
    reasoning_effort: String,
    status: Value,
}

fn emit_collab_spawn_end(deps: &SubagentToolDeps, event: CollabSpawnEnd<'_>) {
    deps.emit(
        "collab_agent_spawn_end",
        json!({
            "call_id": event.call_id,
            "completed_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "new_thread_id": event.new_thread_id,
            "new_agent_nickname": event.new_agent_nickname,
            "new_agent_role": event.new_agent_role,
            "prompt": event.prompt,
            "model": event.model,
            "reasoning_effort": event.reasoning_effort,
            "status": event.status,
        }),
    );
}

fn emit_collab_interaction_begin(
    deps: &SubagentToolDeps,
    ctx: &ToolCtx,
    receiver_thread_id: &str,
    prompt: &str,
) {
    deps.emit(
        "collab_agent_interaction_begin",
        json!({
            "call_id": ctx.call_id,
            "started_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "receiver_thread_id": receiver_thread_id,
            "prompt": prompt,
        }),
    );
}

fn emit_collab_interaction_end(
    deps: &SubagentToolDeps,
    ctx: &ToolCtx,
    target: &AgentEventTarget,
    prompt: &str,
    status: Value,
) {
    deps.emit(
        "collab_agent_interaction_end",
        json!({
            "call_id": ctx.call_id,
            "completed_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "receiver_thread_id": target.thread_id,
            "receiver_agent_nickname": target.nickname,
            "receiver_agent_role": target.role,
            "prompt": prompt,
            "status": status,
        }),
    );
}

fn emit_collab_waiting_begin(
    deps: &SubagentToolDeps,
    ctx: &ToolCtx,
    receiver_thread_ids: Vec<String>,
    receiver_agents: Vec<Value>,
) {
    deps.emit(
        "collab_waiting_begin",
        json!({
            "started_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "receiver_thread_ids": receiver_thread_ids,
            "receiver_agents": receiver_agents,
            "call_id": ctx.call_id,
        }),
    );
}

fn emit_collab_waiting_end(
    deps: &SubagentToolDeps,
    ctx: &ToolCtx,
    statuses: serde_json::Map<String, Value>,
    agent_statuses: Vec<Value>,
) {
    deps.emit(
        "collab_waiting_end",
        json!({
            "sender_thread_id": deps.session_id,
            "call_id": ctx.call_id,
            "completed_at_ms": now_ms(),
            "agent_statuses": agent_statuses,
            "statuses": Value::Object(statuses),
        }),
    );
}

fn emit_collab_close_begin(deps: &SubagentToolDeps, ctx: &ToolCtx, receiver_thread_id: &str) {
    deps.emit(
        "collab_close_begin",
        json!({
            "call_id": ctx.call_id,
            "started_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "receiver_thread_id": receiver_thread_id,
        }),
    );
}

fn emit_collab_close_end(
    deps: &SubagentToolDeps,
    ctx: &ToolCtx,
    target: &AgentEventTarget,
    status: Value,
) {
    deps.emit(
        "collab_close_end",
        json!({
            "call_id": ctx.call_id,
            "completed_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "receiver_thread_id": target.thread_id,
            "receiver_agent_nickname": target.nickname,
            "receiver_agent_role": target.role,
            "status": status,
        }),
    );
}

fn emit_collab_resume_begin(deps: &SubagentToolDeps, ctx: &ToolCtx, target: &AgentEventTarget) {
    deps.emit(
        "collab_resume_begin",
        json!({
            "call_id": ctx.call_id,
            "started_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "receiver_thread_id": target.thread_id,
            "receiver_agent_nickname": target.nickname,
            "receiver_agent_role": target.role,
        }),
    );
}

fn emit_collab_resume_end(
    deps: &SubagentToolDeps,
    ctx: &ToolCtx,
    target: &AgentEventTarget,
    status: Value,
) {
    deps.emit(
        "collab_resume_end",
        json!({
            "call_id": ctx.call_id,
            "completed_at_ms": now_ms(),
            "sender_thread_id": deps.session_id,
            "receiver_thread_id": target.thread_id,
            "receiver_agent_nickname": target.nickname,
            "receiver_agent_role": target.role,
            "status": status,
        }),
    );
}

fn check_store_thread_limit(deps: &SubagentToolDeps) -> Result<(), ToolError> {
    let Some(limit) = deps.max_concurrent_threads_per_session else {
        return Ok(());
    };
    let Some(shared_store) = deps.store.as_ref() else {
        return Ok(());
    };
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let root_id = store_root_session_id(&store, &deps.session_id)
        .map_err(|err| tool_err("resolve root session failed", err))?;
    let live_threads = store_collect_agent_tree(&store, &root_id)
        .map_err(|err| tool_err("collect agent tree failed", err))?
        .into_iter()
        .filter(|agent| agent.status != "closed")
        .count();
    if live_threads >= limit {
        return Err(ToolError::Other(anyhow::anyhow!(
            "max_concurrent_threads_per_session limit reached ({limit})"
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct StoreDelivery {
    agent_path: String,
    agent_id: String,
    nickname: Option<String>,
    role: Option<String>,
    message_id: String,
}

#[derive(Clone, Debug)]
struct AgentEventTarget {
    thread_id: String,
    nickname: Option<String>,
    role: Option<String>,
    status: Option<Value>,
}

fn empty_output() -> ExecOutput {
    ExecOutput {
        exit_code: 0,
        stdout: String::new(),
        stderr: String::new(),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LegacyInputItem {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_elements: Option<Value>,
}

#[derive(Debug, Clone)]
struct LegacyInputPayload {
    preview: String,
    input_items: Option<Value>,
}

fn legacy_input_payload(
    message: Option<&str>,
    items: Option<&[LegacyInputItem]>,
) -> Result<LegacyInputPayload, ToolError> {
    match (message, items) {
        (Some(_), Some(_)) => Err(ToolError::Other(anyhow::anyhow!(
            "Provide either message or items, but not both"
        ))),
        (None, None) => Err(ToolError::Other(anyhow::anyhow!(
            "Provide one of: message or items"
        ))),
        (Some(message), None) => {
            if message.trim().is_empty() {
                return Err(ToolError::Other(anyhow::anyhow!(
                    "Empty message can't be sent to an agent"
                )));
            }
            Ok(LegacyInputPayload {
                preview: message.to_string(),
                input_items: None,
            })
        }
        (None, Some(items)) => {
            if items.is_empty() {
                return Err(ToolError::Other(anyhow::anyhow!("Items can't be empty")));
            }
            let input_items = serde_json::to_value(items).map_err(|err| {
                ToolError::Other(anyhow::anyhow!(
                    "serialize legacy input items failed: {err}"
                ))
            })?;
            let preview = typed_user_input_preview_from_items(&input_items).map_err(|err| {
                ToolError::Other(anyhow::anyhow!("parse legacy input items failed: {err}"))
            })?;
            Ok(LegacyInputPayload {
                preview,
                input_items: Some(input_items),
            })
        }
    }
}

fn child_path(parent: &str, task_name: &str) -> String {
    if parent == "/root" {
        format!("/root/{task_name}")
    } else {
        format!("{}/{task_name}", parent.trim_end_matches('/'))
    }
}

fn next_legacy_task_name(deps: &SubagentToolDeps) -> String {
    let mut used = std::collections::BTreeSet::new();
    used.insert(deps.parent.agent_path.clone());
    for record in deps.manager.registry().list_agents() {
        used.insert(record.agent_path);
    }
    if let Some(shared_store) = deps.store.as_ref() {
        if let Ok(store) = shared_store.lock() {
            if let Ok(root_id) = store_root_session_id(&store, &deps.session_id) {
                if let Ok(agents) = store_collect_agent_tree(&store, &root_id) {
                    for agent in agents {
                        if let Some(path) = agent.agent_path {
                            used.insert(path);
                        }
                    }
                }
            }
        }
    }
    for idx in 1.. {
        let task_name = format!("agent_{idx}");
        if !used.contains(&child_path(&deps.parent.agent_path, &task_name)) {
            return task_name;
        }
    }
    unreachable!("unbounded legacy task name search")
}

fn agent_status_value(status: &AgentStatus) -> Value {
    match status {
        AgentStatus::PendingInit => Value::String("pending_init".to_string()),
        AgentStatus::Running => Value::String("running".to_string()),
        AgentStatus::Interrupted => Value::String("interrupted".to_string()),
        AgentStatus::Completed(result) => json!({ "completed": result }),
        AgentStatus::Errored(error) => json!({ "errored": error }),
        AgentStatus::Shutdown => Value::String("shutdown".to_string()),
        AgentStatus::NotFound => Value::String("not_found".to_string()),
    }
}

fn target_from_store_delivery(delivery: &StoreDelivery) -> AgentEventTarget {
    AgentEventTarget {
        thread_id: delivery.agent_id.clone(),
        nickname: delivery.nickname.clone(),
        role: delivery.role.clone(),
        status: None,
    }
}

fn target_from_record(record: &crate::subagents::registry::AgentRecord) -> AgentEventTarget {
    AgentEventTarget {
        thread_id: record.agent_id.clone(),
        nickname: record.nickname.clone(),
        role: record.role.clone(),
        status: Some(agent_status_value(&record.status)),
    }
}

fn target_from_store_agent(
    deps: &SubagentToolDeps,
    target_id: &str,
) -> Result<AgentEventTarget, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Err(ToolError::Other(anyhow::anyhow!(
            "store-backed target lookup unavailable"
        )));
    };
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let session = store
        .load_session(target_id)
        .map_err(|err| tool_err("load target agent failed", err))?
        .ok_or_else(|| ToolError::Other(anyhow::anyhow!("agent with id {target_id} not found")))?;
    let summary = store
        .agent_summary_for_child(target_id)
        .map_err(|err| tool_err("load target child edge failed", err))?;
    let status = local_agent_status_value(&store, &session, summary.as_ref())
        .map_err(|err| tool_err("read target status failed", err))?;
    Ok(AgentEventTarget {
        thread_id: target_id.to_string(),
        nickname: summary
            .as_ref()
            .and_then(|summary| summary.agent_nickname.clone()),
        role: summary
            .as_ref()
            .and_then(|summary| summary.agent_role.clone()),
        status: Some(status),
    })
}

fn target_from_store_reference_v2(
    deps: &SubagentToolDeps,
    target: &str,
) -> Result<AgentEventTarget, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Err(ToolError::Other(anyhow::anyhow!(
            "store-backed target lookup unavailable"
        )));
    };
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let resolved = store_resolve_agent_reference_in_tree_v2(&store, &deps.session_id, target)
        .map_err(|err| tool_err("resolve agent target failed", err))?
        .ok_or_else(|| ToolError::Other(anyhow::anyhow!("live agent path `{target}` not found")))?;
    let summary = resolved.summary.clone();
    let session = store
        .load_session(&resolved.session_id)
        .map_err(|err| tool_err("load target agent failed", err))?
        .ok_or_else(|| {
            ToolError::Other(anyhow::anyhow!(
                "unknown target session id: {}",
                resolved.session_id
            ))
        })?;
    let status = local_agent_status_value(&store, &session, summary.as_ref())
        .map_err(|err| tool_err("read target status failed", err))?;
    Ok(AgentEventTarget {
        thread_id: resolved.session_id,
        nickname: summary
            .as_ref()
            .and_then(|summary| summary.agent_nickname.clone()),
        role: summary
            .as_ref()
            .and_then(|summary| summary.agent_role.clone()),
        status: Some(status),
    })
}

fn target_from_manager_reference_v2(
    deps: &SubagentToolDeps,
    target: &str,
) -> Result<AgentEventTarget, ToolError> {
    let registry = deps.manager.registry();
    let record = resolve_agent_reference_in_tree_v2(&registry, &deps.parent.agent_path, target)
        .map_err(|err| ToolError::Other(anyhow::anyhow!("resolve agent target failed: {err}")))?
        .ok_or_else(|| ToolError::Other(anyhow::anyhow!("live agent path `{target}` not found")))?;
    Ok(target_from_record(&record))
}

fn target_from_manager_agent_id(
    deps: &SubagentToolDeps,
    target_id: &str,
) -> Result<AgentEventTarget, ToolError> {
    let record = deps
        .manager
        .registry()
        .list_agents()
        .into_iter()
        .find(|record| record.agent_id == target_id)
        .ok_or_else(|| ToolError::Other(anyhow::anyhow!("agent with id {target_id} not found")))?;
    Ok(target_from_record(&record))
}

fn wait_target_event_metadata(
    deps: &SubagentToolDeps,
    targets: &[String],
) -> (Vec<String>, Vec<Value>) {
    let mut receiver_thread_ids = Vec::with_capacity(targets.len());
    let mut receiver_agents = Vec::with_capacity(targets.len());
    for target in targets {
        let metadata = if deps.store.is_some() {
            target_from_store_agent(deps, target).ok()
        } else {
            target_from_manager_agent_id(deps, target).ok()
        };
        receiver_thread_ids.push(target.clone());
        receiver_agents.push(collab_agent_ref(
            target.clone(),
            metadata.as_ref().and_then(|target| target.nickname.clone()),
            metadata.as_ref().and_then(|target| target.role.clone()),
        ));
    }
    (receiver_thread_ids, receiver_agents)
}

fn wait_output_timed_out(output: &ExecOutput) -> bool {
    serde_json::from_str::<Value>(&output.stdout)
        .ok()
        .and_then(|body| body.get("timed_out").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn wait_final_event_statuses(
    deps: &SubagentToolDeps,
    targets: &[String],
    output: &ExecOutput,
) -> (serde_json::Map<String, Value>, Vec<Value>) {
    if wait_output_timed_out(output) {
        return (serde_json::Map::new(), Vec::new());
    }
    let mut statuses = serde_json::Map::new();
    let mut agent_statuses = Vec::new();
    for target_id in targets {
        let target = if deps.store.is_some() {
            target_from_store_agent(deps, target_id).ok()
        } else {
            target_from_manager_agent_id(deps, target_id).ok()
        };
        let Some(target) = target else {
            statuses.insert(target_id.clone(), Value::String("not_found".to_string()));
            agent_statuses.push(collab_agent_status_entry(
                target_id.clone(),
                None,
                None,
                Value::String("not_found".to_string()),
            ));
            continue;
        };
        let Some(status) = target.status.clone() else {
            continue;
        };
        statuses.insert(target.thread_id.clone(), status.clone());
        agent_statuses.push(collab_agent_status_entry(
            target.thread_id,
            target.nickname,
            target.role,
            status,
        ));
    }
    (statuses, agent_statuses)
}

fn tool_err(context: &str, err: impl std::fmt::Display) -> ToolError {
    ToolError::Other(anyhow::anyhow!("{context}: {err}"))
}

fn parent_can_receive_completion_mail(
    store: &browser_use_store::Store,
    parent_session_id: &str,
) -> anyhow::Result<bool> {
    let Some(parent) = store.load_session(parent_session_id)? else {
        return Ok(false);
    };
    if !parent.status.is_active() {
        return Ok(false);
    }
    if store
        .agent_summary_for_child(parent_session_id)?
        .is_some_and(|agent| agent.status == "closed")
    {
        return Ok(false);
    }
    Ok(true)
}

fn format_completion_notification(agent_path: &str, status: &str, payload: &Value) -> String {
    let agent_status = match status {
        "done" => json!({
            "completed": payload.get("result").cloned().unwrap_or(Value::Null),
        }),
        "failed" => json!({
            "errored": payload
                .get("failure")
                .and_then(Value::as_str)
                .unwrap_or("agent failed"),
        }),
        "cancelled" => json!("shutdown"),
        _ => json!("not_found"),
    };
    format!(
        "<subagent_notification>\n{}\n</subagent_notification>",
        json!({
            "agent_path": agent_path,
            "status": agent_status,
        })
    )
}

pub(crate) fn store_completion_handler(
    shared_store: SharedStore,
    parent_session_id: String,
    child_session_id: String,
    run_id: Option<String>,
) -> ChildAgentCompletionHandler {
    ChildAgentCompletionHandler::new(move |completion: ChildAgentRunCompletion| {
        let store = shared_store
            .lock()
            .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
        let events = store.events_for_session(&child_session_id)?;
        if let Some(run_id) = run_id.as_deref() {
            let Some(current_events) = current_child_run_events(&events, run_id) else {
                return Ok(());
            };
            if session_was_interrupted(current_events) {
                return Ok(());
            }
        } else if session_was_interrupted(&events) {
            return Ok(());
        }
        if store
            .agent_summary_for_child(&child_session_id)?
            .is_some_and(|summary| summary.status == "closed")
        {
            return Ok(());
        }
        if parent_has_child_terminal_event_for_run(
            &store,
            &parent_session_id,
            &child_session_id,
            run_id.as_deref(),
        )? {
            return Ok(());
        }
        let status = if completion.success { "done" } else { "failed" };
        let result = completion
            .success
            .then(|| completion.summary.clone())
            .flatten();
        let failure = if completion.success {
            None
        } else {
            Some(
                completion
                    .summary
                    .clone()
                    .unwrap_or_else(|| "child agent failed".to_string()),
            )
        };
        store.set_child_agent_status(&child_session_id, status)?;
        let payload = json!({
            "child_session_id": child_session_id,
            "run_id": run_id.as_deref(),
            "status": status,
            "result": result,
            "failure": failure,
        });
        let event_type = if completion.success {
            "agent.completed"
        } else {
            "agent.failed"
        };
        store.append_event(
            &parent_session_id,
            event_type,
            json!({
                "child_session_id": child_session_id,
                "run_id": run_id.as_deref(),
                "status": status,
                "payload": payload.clone(),
            }),
        )?;
        if parent_can_receive_completion_mail(&store, &parent_session_id)? {
            let child_path = display_agent_path_for_session(&store, &child_session_id)?;
            let notification = format_completion_notification(&child_path, status, &payload);
            store.send_agent_message(
                &child_session_id,
                &parent_session_id,
                &notification,
                false,
            )?;
        }
        Ok(())
    })
}

fn parent_has_child_terminal_event_for_run(
    store: &Store,
    parent_session_id: &str,
    child_session_id: &str,
    run_id: Option<&str>,
) -> anyhow::Result<bool> {
    Ok(store
        .events_for_session(parent_session_id)?
        .iter()
        .any(|event| {
            matches!(
                event.event_type.as_str(),
                "agent.completed" | "agent.failed"
            ) && event
                .payload
                .get("child_session_id")
                .and_then(Value::as_str)
                == Some(child_session_id)
                && match run_id {
                    Some(run_id) => {
                        event
                            .payload
                            .get("run_id")
                            .or_else(|| event.payload.pointer("/payload/run_id"))
                            .and_then(Value::as_str)
                            == Some(run_id)
                    }
                    None => true,
                }
        }))
}

fn current_child_run_events<'a>(
    events: &'a [browser_use_protocol::EventRecord],
    expected_run_id: &str,
) -> Option<&'a [browser_use_protocol::EventRecord]> {
    let marker_idx = events
        .iter()
        .rposition(|event| event.event_type == "agent.run.started")?;
    let marker = &events[marker_idx];
    let marker_run_id = marker.payload.get("run_id").and_then(Value::as_str)?;
    (marker_run_id == expected_run_id).then_some(&events[marker_idx + 1..])
}

fn store_message_tool(
    deps: &SubagentToolDeps,
    target: &str,
    message: &str,
    trigger_turn: bool,
    interrupt: bool,
) -> Result<Option<StoreDelivery>, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Ok(None);
    };
    if message.trim().is_empty() {
        return Err(ToolError::Other(anyhow::anyhow!(
            "Empty message can't be sent to an agent"
        )));
    }

    let (delivery, wake_request) = {
        let store = shared_store
            .lock()
            .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
        let target = store_resolve_agent_reference_in_tree_v2(&store, &deps.session_id, target)
            .map_err(|err| tool_err("resolve agent target failed", err))?
            .ok_or_else(|| {
                ToolError::Other(anyhow::anyhow!("live agent path `{target}` not found"))
            })?;
        if trigger_turn && target.is_root {
            return Err(ToolError::Other(anyhow::anyhow!(
                "Tasks can't be assigned to the root agent"
            )));
        }
        let target_status = store
            .load_session(&target.session_id)
            .map_err(|err| tool_err("load target agent failed", err))?
            .map(|session| session.status);
        if interrupt
            && target_status
                .as_ref()
                .is_some_and(browser_use_protocol::SessionStatus::is_active)
        {
            store
                .request_cancel(&target.session_id, "interrupted by send_input")
                .map_err(|err| tool_err("interrupt target agent failed", err))?;
        }
        let msg = store
            .send_agent_message(&deps.session_id, &target.session_id, message, trigger_turn)
            .map_err(|err| tool_err("send agent message failed", err))?;
        let author_path = display_agent_path_for_session(&store, &deps.session_id)
            .map_err(|err| tool_err("resolve author path failed", err))?;
        store
            .append_event(
                &deps.session_id,
                "agent.message",
                json!({
                    "id": msg.id,
                    "author_session_id": msg.author_session_id,
                    "target_session_id": msg.target_session_id,
                    "author_path": author_path,
                    "recipient_path": target.agent_path,
                    "child_session_id": target.session_id,
                    "content": msg.content,
                    "input_items": msg.input_items,
                    "input_kind": msg.input_kind,
                    "trigger_turn": msg.trigger_turn,
                    "interrupt": interrupt,
                }),
            )
            .map_err(|err| tool_err("record agent message failed", err))?;
        let target_is_running = store
            .load_session(&target.session_id)
            .map_err(|err| tool_err("load target agent failed", err))?
            .is_some_and(|session| session.status == browser_use_protocol::SessionStatus::Running);
        let wake_request = if trigger_turn && !target_is_running {
            target.summary.as_ref().map(|summary| {
                let run_id = browser_use_store::new_thread_id();
                ChildAgentRunRequest {
                    parent_session_id: summary.parent_session_id.clone(),
                    child_session_id: target.session_id.clone(),
                    run_id: Some(run_id.clone()),
                    message: message.to_string(),
                    input_items: None,
                    input_is_inter_agent_communication: false,
                    agent_path: Some(target.agent_path.clone()),
                    nickname: summary.agent_nickname.clone(),
                    role: summary.agent_role.clone(),
                    fork_turns: Some("none".to_string()),
                    model: None,
                    reasoning_effort: None,
                    service_tier: None,
                    config_overrides: Vec::new(),
                    completion_handler: Some(store_completion_handler(
                        Arc::clone(shared_store),
                        summary.parent_session_id.clone(),
                        target.session_id.clone(),
                        Some(run_id),
                    )),
                }
            })
        } else {
            None
        };
        let delivery = StoreDelivery {
            agent_path: target.agent_path.clone(),
            agent_id: target.session_id.clone(),
            nickname: target
                .summary
                .as_ref()
                .and_then(|summary| summary.agent_nickname.clone()),
            role: target
                .summary
                .as_ref()
                .and_then(|summary| summary.agent_role.clone()),
            message_id: msg.id,
        };
        (delivery, wake_request)
    };
    if let (Some(runner), Some(request)) = (deps.child_runner.as_ref(), wake_request) {
        if interrupt {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let store = shared_store
                .lock()
                .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
            store
                .set_status(
                    &request.child_session_id,
                    browser_use_protocol::SessionStatus::Created,
                )
                .map_err(|err| tool_err("reset interrupted target agent failed", err))?;
        }
        runner
            .run(request)
            .map_err(|err| tool_err("trigger target agent failed", err))?;
    }
    Ok(Some(delivery))
}

fn legacy_agent_id_target(target: &str) -> Result<&str, ToolError> {
    if target.is_empty() {
        return Err(ToolError::Other(anyhow::anyhow!(
            "agent id must not be empty"
        )));
    }
    if target.contains('/') {
        return Err(ToolError::Other(anyhow::anyhow!(
            "invalid agent id {target}: legacy multi-agent tools target agent ids, not paths"
        )));
    }
    if !browser_use_store::is_thread_id(target) {
        return Err(ToolError::Other(anyhow::anyhow!(
            "invalid agent id {target}: expected UUID thread id"
        )));
    }
    Ok(target)
}

fn legacy_agent_id_targets(targets: &[String]) -> Result<(), ToolError> {
    if targets.is_empty() {
        return Err(ToolError::Other(anyhow::anyhow!(
            "agent ids must be non-empty"
        )));
    }
    for target in targets {
        legacy_agent_id_target(target)?;
    }
    Ok(())
}

fn reject_legacy_depth_limit(deps: &SubagentToolDeps) -> Result<(), ToolError> {
    check_spawn_depth(deps.parent.depth, deps.manager.max_depth())
        .map(|_| ())
        .map_err(|_| {
            ToolError::Other(anyhow::anyhow!(
                "Agent depth limit reached. Solve the task yourself."
            ))
        })
}

fn resumable_child_state(
    session_status: browser_use_protocol::SessionStatus,
    edge_status: &str,
) -> bool {
    matches!(
        session_status,
        browser_use_protocol::SessionStatus::Done
            | browser_use_protocol::SessionStatus::Failed
            | browser_use_protocol::SessionStatus::Cancelled
    ) || matches!(edge_status, "closed" | "done" | "failed")
}

fn store_message_tool_v1(
    deps: &SubagentToolDeps,
    target: &str,
    message: &str,
    input_items: Option<Value>,
    interrupt: bool,
) -> Result<Option<StoreDelivery>, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Ok(None);
    };
    let target = legacy_agent_id_target(target)?;
    if message.trim().is_empty() {
        return Err(ToolError::Other(anyhow::anyhow!(
            "Empty message can't be sent to an agent"
        )));
    }

    let (delivery, wake_request) = {
        let store = shared_store
            .lock()
            .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
        let target_session = store
            .load_session(target)
            .map_err(|err| tool_err("load target agent failed", err))?
            .ok_or_else(|| ToolError::Other(anyhow::anyhow!("agent with id {target} not found")))?;
        if interrupt && target_session.status.is_active() {
            store
                .request_cancel(target, "interrupted by send_input")
                .map_err(|err| tool_err("interrupt target agent failed", err))?;
        }
        let msg = store
            .send_agent_message_with_input_items(
                &deps.session_id,
                target,
                message,
                input_items.clone(),
                true,
            )
            .map_err(|err| tool_err("send agent message failed", err))?;
        let author_path = display_agent_path_for_session(&store, &deps.session_id)
            .map_err(|err| tool_err("resolve author path failed", err))?;
        let recipient_path = display_agent_path_for_session(&store, target)
            .map_err(|err| tool_err("resolve recipient path failed", err))?;
        store
            .append_event(
                &deps.session_id,
                "agent.message",
                json!({
                    "id": msg.id,
                    "author_session_id": msg.author_session_id,
                    "target_session_id": msg.target_session_id,
                    "author_path": author_path,
                    "recipient_path": recipient_path,
                    "child_session_id": target,
                    "content": msg.content,
                    "input_items": msg.input_items,
                    "input_kind": msg.input_kind,
                    "trigger_turn": msg.trigger_turn,
                    "interrupt": interrupt,
                }),
            )
            .map_err(|err| tool_err("record agent message failed", err))?;
        let target_is_running = store
            .load_session(target)
            .map_err(|err| tool_err("load target agent failed", err))?
            .is_some_and(|session| session.status == browser_use_protocol::SessionStatus::Running);
        let summary = store
            .agent_summary_for_child(target)
            .map_err(|err| tool_err("load target child edge failed", err))?;
        let wake_request = if !target_is_running {
            summary.as_ref().map(|summary| {
                let run_id = browser_use_store::new_thread_id();
                ChildAgentRunRequest {
                    parent_session_id: summary.parent_session_id.clone(),
                    child_session_id: target.to_string(),
                    run_id: Some(run_id.clone()),
                    message: message.to_string(),
                    input_items: input_items.clone(),
                    input_is_inter_agent_communication: false,
                    agent_path: Some(recipient_path.clone()),
                    nickname: summary.agent_nickname.clone(),
                    role: summary.agent_role.clone(),
                    fork_turns: Some("none".to_string()),
                    model: None,
                    reasoning_effort: None,
                    service_tier: None,
                    config_overrides: Vec::new(),
                    completion_handler: Some(store_completion_handler(
                        Arc::clone(shared_store),
                        summary.parent_session_id.clone(),
                        target.to_string(),
                        Some(run_id),
                    )),
                }
            })
        } else {
            None
        };
        (
            StoreDelivery {
                agent_path: recipient_path,
                agent_id: target.to_string(),
                nickname: summary
                    .as_ref()
                    .and_then(|summary| summary.agent_nickname.clone()),
                role: summary
                    .as_ref()
                    .and_then(|summary| summary.agent_role.clone()),
                message_id: msg.id,
            },
            wake_request,
        )
    };
    if let (Some(runner), Some(request)) = (deps.child_runner.as_ref(), wake_request) {
        if interrupt {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let store = shared_store
                .lock()
                .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
            store
                .set_status(
                    &request.child_session_id,
                    browser_use_protocol::SessionStatus::Created,
                )
                .map_err(|err| tool_err("reset interrupted target agent failed", err))?;
        }
        runner
            .run(request)
            .map_err(|err| tool_err("trigger target agent failed", err))?;
    }
    Ok(Some(delivery))
}

fn store_resume_requests_for_agent_subtree(
    shared_store: &SharedStore,
    root_child_id: &str,
) -> Result<Vec<ChildAgentRunRequest>, ToolError> {
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let mut queue = std::collections::VecDeque::from([root_child_id.to_string()]);
    let mut requests = Vec::new();
    while let Some(child_id) = queue.pop_front() {
        let Some(summary) = store
            .agent_summary_for_child(&child_id)
            .map_err(|err| tool_err("load resumed child edge failed", err))?
        else {
            continue;
        };
        if child_id != root_child_id && summary.status != "open" {
            continue;
        }
        let Some(session) = store
            .load_session(&child_id)
            .map_err(|err| tool_err("load resumed child failed", err))?
        else {
            continue;
        };
        let agent_path = display_agent_path_for_session(&store, &child_id)
            .map_err(|err| tool_err("resolve resumed child path failed", err))?;
        let message = last_task_message_for_agent(&store, &child_id)
            .map_err(|err| tool_err("read resumed child task failed", err))?
            .unwrap_or_default();
        if matches!(
            session.status,
            browser_use_protocol::SessionStatus::Created
                | browser_use_protocol::SessionStatus::Running
                | browser_use_protocol::SessionStatus::Cancelled
        ) {
            let run_id = browser_use_store::new_thread_id();
            requests.push(ChildAgentRunRequest {
                parent_session_id: summary.parent_session_id.clone(),
                child_session_id: child_id.clone(),
                run_id: Some(run_id.clone()),
                message,
                input_items: None,
                input_is_inter_agent_communication: false,
                agent_path: Some(agent_path),
                nickname: summary.agent_nickname.clone(),
                role: summary.agent_role.clone(),
                fork_turns: Some("none".to_string()),
                model: None,
                reasoning_effort: None,
                service_tier: None,
                config_overrides: Vec::new(),
                completion_handler: Some(store_completion_handler(
                    Arc::clone(shared_store),
                    summary.parent_session_id.clone(),
                    child_id.clone(),
                    Some(run_id),
                )),
            });
        }
        for child in store
            .list_child_agents(&child_id)
            .map_err(|err| tool_err("list resumed descendants failed", err))?
        {
            if child.status == "open" {
                queue.push_back(child.child_session_id);
            }
        }
    }
    Ok(requests)
}

fn agent_path_matches_prefix(agent_path: &str, prefix: &str) -> bool {
    agent_path == prefix
        || agent_path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn store_list_agents(
    deps: &SubagentToolDeps,
    path_prefix: Option<&str>,
) -> Result<Option<ExecOutput>, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Ok(None);
    };
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let root_id = store_root_session_id(&store, &deps.session_id)
        .map_err(|err| tool_err("resolve root agent failed", err))?;
    let current_path = display_agent_path_for_session(&store, &deps.session_id)
        .map_err(|err| tool_err("resolve current agent path failed", err))?;
    let prefix = path_prefix
        .map(|prefix| resolve_agent_path_v2(&current_path, prefix))
        .transpose()
        .map_err(|err| tool_err("resolve agent prefix failed", err))?;

    let mut agents = Vec::new();
    if prefix
        .as_deref()
        .is_none_or(|prefix| prefix == "/root" || agent_path_matches_prefix("/root", prefix))
    {
        let root = store
            .load_session(&root_id)
            .map_err(|err| tool_err("load root agent failed", err))?
            .ok_or_else(|| {
                ToolError::Other(anyhow::anyhow!("unknown root session id: {root_id}"))
            })?;
        let status = local_agent_status_value(&store, &root, None)
            .map_err(|err| tool_err("read root status failed", err))?;
        agents.push(json!({
            "agent_name": "/root",
            "agent_status": status,
            "last_task_message": "Main thread",
        }));
    }

    for agent in store_collect_agent_tree(&store, &root_id)
        .map_err(|err| tool_err("collect agent tree failed", err))?
        .into_iter()
        .filter(|agent| agent.status != "closed")
    {
        let child = store
            .load_session(&agent.child_session_id)
            .map_err(|err| tool_err("load child agent failed", err))?
            .ok_or_else(|| {
                ToolError::Other(anyhow::anyhow!(
                    "unknown child session id: {}",
                    agent.child_session_id
                ))
            })?;
        let agent_path = agent.agent_path.clone().unwrap_or_else(|| {
            display_agent_path_for_session(&store, &agent.child_session_id)
                .unwrap_or_else(|_| agent.child_session_id.clone())
        });
        if prefix
            .as_deref()
            .is_some_and(|prefix| !agent_path_matches_prefix(&agent_path, prefix))
        {
            continue;
        }
        let status = local_agent_status_value(&store, &child, Some(&agent))
            .map_err(|err| tool_err("read child status failed", err))?;
        let last_task_message = last_task_message_for_agent(&store, &child.id)
            .map_err(|err| tool_err("read last task failed", err))?;
        agents.push(json!({
            "agent_name": agent_path,
            "agent_status": status,
            "last_task_message": last_task_message,
        }));
    }
    agents.sort_by(|left, right| {
        left.get("agent_name")
            .and_then(Value::as_str)
            .cmp(&right.get("agent_name").and_then(Value::as_str))
    });
    Ok(Some(ok_output(json!({ "agents": agents }))))
}

async fn store_wait_agent(
    deps: &SubagentToolDeps,
    agent_path: Option<&str>,
    timeout: Duration,
) -> Result<Option<ExecOutput>, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Ok(None);
    };
    let (watcher, mut cursor) = store_notification_watcher(shared_store)?;
    let deadline = Instant::now() + timeout;
    if let Some(target) = agent_path {
        let (target_id, target_path) = {
            let store = shared_store
                .lock()
                .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
            let resolved = store_resolve_agent_reference_in_tree(&store, &deps.session_id, target)
                .map_err(|err| tool_err("resolve agent target failed", err))?
                .ok_or_else(|| {
                    ToolError::Other(anyhow::anyhow!("live agent path `{target}` not found"))
                })?;
            (resolved.session_id, resolved.agent_path)
        };
        loop {
            let status = {
                let store = shared_store
                    .lock()
                    .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
                let statuses = final_statuses_for_v1_wait(&store, &[target_id.as_str()])
                    .map_err(|err| tool_err("read target status failed", err))?;
                statuses.values().next().cloned()
            };
            if let Some(status) = status {
                return Ok(Some(ok_output(json!({
                    "agent_path": target_path,
                    "agent_id": target_id,
                    "status": status,
                    "timed_out": false,
                }))));
            }
            if Instant::now() >= deadline
                || !wait_for_store_change(&watcher, &mut cursor, deadline).await?
            {
                return Ok(Some(ok_output(json!({
                    "agent_path": target_path,
                    "agent_id": target_id,
                    "status": "running",
                    "timed_out": true,
                }))));
            }
        }
    }

    loop {
        let has_mail = {
            let store = shared_store
                .lock()
                .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
            !store
                .messages_for_agent(&deps.session_id)
                .map_err(|err| tool_err("read agent mailbox failed", err))?
                .is_empty()
        };
        if has_mail {
            return Ok(Some(ok_output(json!({
                "message": "Wait completed.",
                "timed_out": false,
            }))));
        }
        if Instant::now() >= deadline
            || !wait_for_store_change(&watcher, &mut cursor, deadline).await?
        {
            return Ok(Some(ok_output(json!({
                "message": "Wait timed out.",
                "timed_out": true,
            }))));
        }
    }
}

async fn store_wait_agent_v1(
    deps: &SubagentToolDeps,
    targets: &[String],
    timeout: Duration,
) -> Result<Option<ExecOutput>, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Ok(None);
    };
    let (watcher, mut cursor) = store_notification_watcher(shared_store)?;
    let target_refs = targets.iter().map(String::as_str).collect::<Vec<_>>();
    let deadline = Instant::now() + timeout;
    loop {
        let statuses = {
            let store = shared_store
                .lock()
                .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
            final_statuses_for_v1_wait(&store, &target_refs)
                .map_err(|err| tool_err("read target statuses failed", err))?
        };
        if !statuses.is_empty() || Instant::now() >= deadline {
            return Ok(Some(ok_output(json!({
                "status": Value::Object(statuses.clone()),
                "timed_out": statuses.is_empty(),
            }))));
        }
        if !wait_for_store_change(&watcher, &mut cursor, deadline).await? {
            return Ok(Some(ok_output(json!({
                "status": {},
                "timed_out": true,
            }))));
        }
    }
}

fn wait_timeout(
    requested_ms: Option<i64>,
    options: WaitAgentTimeoutOptions,
) -> Result<Duration, ToolError> {
    let timeout_ms = match requested_ms {
        Some(ms) if ms < options.min_timeout_ms => {
            return Err(ToolError::Other(anyhow::anyhow!(
                "timeout_ms must be at least {}",
                options.min_timeout_ms
            )));
        }
        Some(ms) if ms > options.max_timeout_ms => {
            return Err(ToolError::Other(anyhow::anyhow!(
                "timeout_ms must be at most {}",
                options.max_timeout_ms
            )));
        }
        Some(ms) => ms as u64,
        None => options.default_timeout_ms as u64,
    };
    Ok(Duration::from_millis(timeout_ms))
}

fn wait_timeout_v1(
    requested_ms: Option<i64>,
    options: WaitAgentTimeoutOptions,
) -> Result<Duration, ToolError> {
    let timeout_ms = requested_ms.unwrap_or(options.default_timeout_ms);
    if timeout_ms <= 0 {
        return Err(ToolError::Other(anyhow::anyhow!(
            "timeout_ms must be greater than zero"
        )));
    }
    let timeout_ms = timeout_ms.clamp(options.min_timeout_ms, options.max_timeout_ms);
    Ok(Duration::from_millis(timeout_ms as u64))
}

fn store_notification_watcher(
    shared_store: &SharedStore,
) -> Result<(StoreNotificationWatcher, u64), ToolError> {
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let watcher = store.notification_watcher();
    let cursor = watcher.cursor();
    Ok((watcher, cursor))
}

async fn wait_for_store_change(
    watcher: &StoreNotificationWatcher,
    cursor: &mut u64,
    deadline: Instant,
) -> Result<bool, ToolError> {
    if Instant::now() >= deadline {
        return Ok(false);
    }
    let timeout = deadline.saturating_duration_since(Instant::now());
    let wait_cursor = *cursor;
    let watcher = watcher.clone();
    let (changed, next_cursor) = tokio::task::spawn_blocking(move || {
        let changed = watcher.wait_for_change(wait_cursor, timeout);
        let next_cursor = watcher.cursor();
        (changed, next_cursor)
    })
    .await
    .map_err(|err| ToolError::Other(anyhow::anyhow!("store notification wait failed: {err}")))?;
    *cursor = next_cursor;
    Ok(changed)
}

fn wait_finished_payload(output: &ExecOutput) -> Value {
    let body = serde_json::from_str::<Value>(&output.stdout).unwrap_or(Value::Null);
    json!({
        "timed_out": body
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "status": body.get("status").cloned(),
        "message": body.get("message").cloned(),
    })
}

fn store_close_agent(
    deps: &SubagentToolDeps,
    target: &str,
) -> Result<Option<ExecOutput>, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Ok(None);
    };
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let target = store_resolve_agent_reference_in_tree_v2(&store, &deps.session_id, target)
        .map_err(|err| tool_err("resolve close target failed", err))?
        .ok_or_else(|| ToolError::Other(anyhow::anyhow!("live agent path `{target}` not found")))?;
    if target.is_root {
        return Err(ToolError::Other(anyhow::anyhow!(
            "root is not a spawned agent"
        )));
    }
    let child = store
        .load_session(&target.session_id)
        .map_err(|err| tool_err("load close target failed", err))?
        .ok_or_else(|| {
            ToolError::Other(anyhow::anyhow!(
                "unknown child session id: {}",
                target.session_id
            ))
        })?;
    let summary = store
        .agent_summary_for_child(&target.session_id)
        .map_err(|err| tool_err("load child edge failed", err))?
        .ok_or_else(|| {
            ToolError::Other(anyhow::anyhow!(
                "unknown child agent edge for session id: {}",
                target.session_id
            ))
        })?;
    let previous_status = local_agent_status_value(&store, &child, Some(&summary))
        .map_err(|err| tool_err("read previous status failed", err))?;
    cleanup_agent_runtime_state_for_agent_subtree(&store, &target.session_id, |_| 0)
        .map_err(|err| tool_err("cleanup close target failed", err))?;
    store
        .close_child_agent(&target.session_id, "closed by close_agent")
        .map_err(|err| tool_err("close child agent failed", err))?;
    store
        .append_event(
            &summary.parent_session_id,
            "agent.cancelled",
            json!({
                "child_session_id": target.session_id,
                "status": "cancelled",
                "payload": { "reason": "closed by close_agent" },
            }),
        )
        .map_err(|err| tool_err("record close event failed", err))?;
    Ok(Some(ok_output(json!({
        "previous_status": previous_status,
    }))))
}

fn store_close_agent_v1(
    deps: &SubagentToolDeps,
    target: &str,
) -> Result<Option<ExecOutput>, ToolError> {
    let Some(shared_store) = deps.store.as_ref() else {
        return Ok(None);
    };
    let target = legacy_agent_id_target(target)?;
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let child = store
        .load_session(target)
        .map_err(|err| tool_err("load close target failed", err))?
        .ok_or_else(|| ToolError::Other(anyhow::anyhow!("agent with id {target} not found")))?;
    let summary = store
        .agent_summary_for_child(target)
        .map_err(|err| tool_err("load child edge failed", err))?
        .ok_or_else(|| ToolError::Other(anyhow::anyhow!("root is not a spawned agent")))?;
    let previous_status = local_agent_status_value(&store, &child, Some(&summary))
        .map_err(|err| tool_err("read previous status failed", err))?;
    cleanup_agent_runtime_state_for_agent_subtree(&store, target, |_| 0)
        .map_err(|err| tool_err("cleanup close target failed", err))?;
    store
        .close_child_agent(target, "closed by close_agent")
        .map_err(|err| tool_err("close child agent failed", err))?;
    store
        .append_event(
            &summary.parent_session_id,
            "agent.cancelled",
            json!({
                "child_session_id": target,
                "status": "cancelled",
                "payload": { "reason": "closed by close_agent" },
            }),
        )
        .map_err(|err| tool_err("record close event failed", err))?;
    Ok(Some(ok_output(json!({
        "previous_status": previous_status,
    }))))
}

// ----------------------------------------------------------------------------
// spawn_agent
// ----------------------------------------------------------------------------

/// The `spawn_agent` tool: delegate a task to a freshly-spawned child agent.
///
/// The model's args are [`SpawnAgentArgs`] (`task_name` + `message`, with the
/// optional `agent_type` / `model` / `reasoning_effort` / `service_tier` /
/// `fork_turns` overrides). On success it returns the new child's
/// `{ agent_path, agent_id }` so the model can later `wait_agent` / `send_input`.
pub struct SpawnAgentTool {
    deps: SubagentToolDeps,
}

impl SpawnAgentTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<SpawnAgentArgs> for SpawnAgentTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &SpawnAgentArgs) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for SpawnAgentTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<SpawnAgentArgs, ExecOutput> for SpawnAgentTool {
    fn parallel_safe(&self, _req: &SpawnAgentArgs) -> bool {
        false
    }

    async fn run(
        &self,
        req: &SpawnAgentArgs,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let _spawn_gate = self.deps.spawn_gate.lock().await;
        check_store_thread_limit(&self.deps)?;
        let mut args = req.clone();
        args.input_is_inter_agent_communication = true;
        let prompt = args.message.clone();
        let requested_model = requested_model(args.model.as_ref());
        let requested_reasoning_effort = requested_reasoning_effort(args.reasoning_effort.as_ref());
        emit_collab_spawn_begin(
            &self.deps,
            ctx,
            &prompt,
            requested_model,
            requested_reasoning_effort,
        );
        match self
            .deps
            .manager
            .spawn(args.clone(), &self.deps.parent)
            .await
        {
            Ok(handle) => {
                let agent_path = handle.agent_path.clone();
                let nickname = self
                    .deps
                    .manager
                    .registry()
                    .get(&agent_path)
                    .and_then(|record| record.nickname);
                let role = self
                    .deps
                    .manager
                    .registry()
                    .get(&agent_path)
                    .and_then(|record| record.role);
                emit_collab_spawn_end(
                    &self.deps,
                    CollabSpawnEnd {
                        call_id: &ctx.call_id,
                        new_thread_id: Some(handle.agent_id.clone()),
                        new_agent_nickname: nickname.clone(),
                        new_agent_role: role.clone(),
                        prompt: &prompt,
                        model: effective_spawn_model(&self.deps, args.model.as_ref()),
                        reasoning_effort: effective_spawn_reasoning_effort(
                            &self.deps,
                            args.reasoning_effort.as_ref(),
                        ),
                        status: Value::String("running".to_string()),
                    },
                );
                self.deps.emit(
                    "subagent.spawned",
                    json!({
                        "agent_path": agent_path.clone(),
                        "agent_id": handle.agent_id.clone(),
                        "task_name": args.task_name.clone(),
                        "message": args.message.clone(),
                    }),
                );
                if self.deps.store.is_none() {
                    self.deps.emit(
                        "agent.spawned",
                        json!({
                            "child_session_id": handle.agent_id.clone(),
                            "agent_path": agent_path.clone(),
                            "nickname": nickname.clone(),
                            "role": role.clone(),
                        }),
                    );
                }
                let body = if self.deps.hide_spawn_agent_metadata {
                    json!({ "task_name": agent_path.clone() })
                } else {
                    json!({
                        "task_name": agent_path.clone(),
                        "nickname": nickname.clone(),
                    })
                };
                Ok(ok_output(body))
            }
            // A spawn rejection (depth exceeded, bad task_name/fork_turns, spawner
            // failure) is surfaced to the model as a tool error naming the cause —
            // NOT a panic, matching codex's handler rejection.
            Err(err) => {
                emit_collab_spawn_end(
                    &self.deps,
                    CollabSpawnEnd {
                        call_id: &ctx.call_id,
                        new_thread_id: None,
                        new_agent_nickname: None,
                        new_agent_role: None,
                        prompt: &prompt,
                        model: effective_spawn_model(&self.deps, args.model.as_ref()),
                        reasoning_effort: effective_spawn_reasoning_effort(
                            &self.deps,
                            args.reasoning_effort.as_ref(),
                        ),
                        status: Value::String("not_found".to_string()),
                    },
                );
                Err(ToolError::Other(anyhow::anyhow!(
                    "spawn_agent failed: {err}"
                )))
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpawnAgentV1Request {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub items: Option<Vec<LegacyInputItem>>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub fork_context: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
}

pub struct SpawnAgentV1Tool {
    deps: SubagentToolDeps,
}

impl SpawnAgentV1Tool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<SpawnAgentV1Request> for SpawnAgentV1Tool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &SpawnAgentV1Request) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for SpawnAgentV1Tool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<SpawnAgentV1Request, ExecOutput> for SpawnAgentV1Tool {
    fn parallel_safe(&self, _req: &SpawnAgentV1Request) -> bool {
        false
    }

    async fn run(
        &self,
        req: &SpawnAgentV1Request,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let _spawn_gate = self.deps.spawn_gate.lock().await;
        check_store_thread_limit(&self.deps)?;
        reject_legacy_depth_limit(&self.deps)?;
        let input = legacy_input_payload(req.message.as_deref(), req.items.as_deref())?;
        let args = SpawnAgentArgs {
            message: input.preview,
            task_name: next_legacy_task_name(&self.deps),
            input_items: input.input_items,
            input_is_inter_agent_communication: false,
            agent_type: req.agent_type.clone(),
            model: req.model.clone(),
            reasoning_effort: req.reasoning_effort.clone(),
            service_tier: req.service_tier.clone(),
            fork_turns: Some(if req.fork_context.unwrap_or(false) {
                "all".to_string()
            } else {
                "none".to_string()
            }),
            fork_context: None,
        };
        let prompt = args.message.clone();
        emit_collab_spawn_begin(
            &self.deps,
            ctx,
            &prompt,
            requested_model(args.model.as_ref()),
            requested_reasoning_effort(args.reasoning_effort.as_ref()),
        );
        match self
            .deps
            .manager
            .spawn(args.clone(), &self.deps.parent)
            .await
        {
            Ok(handle) => {
                let nickname = self
                    .deps
                    .manager
                    .registry()
                    .get(&handle.agent_path)
                    .and_then(|record| record.nickname);
                let role = self
                    .deps
                    .manager
                    .registry()
                    .get(&handle.agent_path)
                    .and_then(|record| record.role);
                emit_collab_spawn_end(
                    &self.deps,
                    CollabSpawnEnd {
                        call_id: &ctx.call_id,
                        new_thread_id: Some(handle.agent_id.clone()),
                        new_agent_nickname: nickname.clone(),
                        new_agent_role: role.clone(),
                        prompt: &prompt,
                        model: effective_spawn_model(&self.deps, args.model.as_ref()),
                        reasoning_effort: effective_spawn_reasoning_effort(
                            &self.deps,
                            args.reasoning_effort.as_ref(),
                        ),
                        status: Value::String("running".to_string()),
                    },
                );
                self.deps.emit(
                    "subagent.spawned",
                    json!({
                        "agent_path": handle.agent_path.clone(),
                        "agent_id": handle.agent_id.clone(),
                        "task_name": args.task_name.clone(),
                        "message": args.message.clone(),
                    }),
                );
                if self.deps.store.is_none() {
                    self.deps.emit(
                        "agent.spawned",
                        json!({
                            "child_session_id": handle.agent_id.clone(),
                            "agent_path": handle.agent_path.clone(),
                            "nickname": nickname.clone(),
                            "role": role.clone(),
                        }),
                    );
                }
                Ok(ok_output(json!({
                    "agent_id": handle.agent_id,
                        "nickname": nickname.clone(),
                })))
            }
            Err(err) => {
                emit_collab_spawn_end(
                    &self.deps,
                    CollabSpawnEnd {
                        call_id: &ctx.call_id,
                        new_thread_id: None,
                        new_agent_nickname: None,
                        new_agent_role: None,
                        prompt: &prompt,
                        model: effective_spawn_model(&self.deps, args.model.as_ref()),
                        reasoning_effort: effective_spawn_reasoning_effort(
                            &self.deps,
                            args.reasoning_effort.as_ref(),
                        ),
                        status: Value::String("not_found".to_string()),
                    },
                );
                Err(ToolError::Other(anyhow::anyhow!(
                    "spawn_agent failed: {err}"
                )))
            }
        }
    }
}

// ----------------------------------------------------------------------------
// wait_agent
// ----------------------------------------------------------------------------

/// Wire args for `wait_agent`: the agent to wait on + an optional timeout.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitAgentRequest {
    /// Optional wait budget in milliseconds. Codex v2's wait is targetless.
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

/// The `wait_agent` tool: EVENT-NOTIFY wait until the child has news, then
/// report its status. Routes into [`SubagentManager::wait`] (no poll loop).
pub struct WaitAgentTool {
    deps: SubagentToolDeps,
}

impl WaitAgentTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<WaitAgentRequest> for WaitAgentTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &WaitAgentRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for WaitAgentTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<WaitAgentRequest, ExecOutput> for WaitAgentTool {
    fn parallel_safe(&self, _req: &WaitAgentRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &WaitAgentRequest,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let timeout = wait_timeout(req.timeout_ms, self.deps.wait_timeouts)?;
        emit_collab_waiting_begin(&self.deps, ctx, Vec::new(), Vec::new());
        self.deps.emit(
            "agent.wait.started",
            json!({
                "timeout_ms": timeout.as_millis() as u64,
                "tool": "wait_agent",
            }),
        );
        if self.deps.store.is_some() {
            let output = store_wait_agent(&self.deps, None, timeout)
                .await?
                .ok_or_else(|| {
                    ToolError::Other(anyhow::anyhow!(
                        "store-backed wait unexpectedly unavailable"
                    ))
                })?;
            emit_collab_waiting_end(&self.deps, ctx, serde_json::Map::new(), Vec::new());
            self.deps
                .emit("agent.wait.finished", wait_finished_payload(&output));
            return Ok(output);
        }
        let woken = self.deps.manager.wait_any(timeout).await;
        let output = ok_output(json!({
            "message": if woken { "Wait completed." } else { "Wait timed out." },
            "timed_out": !woken,
        }));
        emit_collab_waiting_end(&self.deps, ctx, serde_json::Map::new(), Vec::new());
        self.deps
            .emit("agent.wait.finished", wait_finished_payload(&output));
        Ok(output)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WaitAgentV1Request {
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

pub struct WaitAgentV1Tool {
    deps: SubagentToolDeps,
}

impl WaitAgentV1Tool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<WaitAgentV1Request> for WaitAgentV1Tool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &WaitAgentV1Request) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for WaitAgentV1Tool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<WaitAgentV1Request, ExecOutput> for WaitAgentV1Tool {
    fn parallel_safe(&self, _req: &WaitAgentV1Request) -> bool {
        false
    }

    async fn run(
        &self,
        req: &WaitAgentV1Request,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        legacy_agent_id_targets(&req.targets)?;
        let timeout = wait_timeout_v1(req.timeout_ms, self.deps.wait_timeouts)?;
        let (receiver_thread_ids, receiver_agents) =
            wait_target_event_metadata(&self.deps, &req.targets);
        emit_collab_waiting_begin(&self.deps, ctx, receiver_thread_ids, receiver_agents);
        self.deps.emit(
            "agent.wait.started",
            json!({
                "targets": req.targets.clone(),
                "timeout_ms": timeout.as_millis() as u64,
                "tool": "wait_agent",
            }),
        );
        if let Some(output) = store_wait_agent_v1(&self.deps, &req.targets, timeout).await? {
            let (statuses, agent_statuses) =
                wait_final_event_statuses(&self.deps, &req.targets, &output);
            emit_collab_waiting_end(&self.deps, ctx, statuses, agent_statuses);
            self.deps
                .emit("agent.wait.finished", wait_finished_payload(&output));
            return Ok(output);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let mut statuses = serde_json::Map::new();
            for target in &req.targets {
                let record = self
                    .deps
                    .manager
                    .registry()
                    .list_agents()
                    .into_iter()
                    .find(|record| record.agent_id == *target);
                let key = record
                    .as_ref()
                    .map(|record| record.agent_path.clone())
                    .unwrap_or_else(|| target.clone());
                let status = record
                    .map(|record| record.status)
                    .unwrap_or(AgentStatus::NotFound);
                if matches!(
                    status,
                    AgentStatus::Completed(_)
                        | AgentStatus::Errored(_)
                        | AgentStatus::Shutdown
                        | AgentStatus::NotFound
                ) {
                    statuses.insert(key, agent_status_value(&status));
                }
            }
            if !statuses.is_empty() || Instant::now() >= deadline {
                let timed_out = statuses.is_empty();
                let output = ok_output(json!({
                    "status": Value::Object(statuses),
                    "timed_out": timed_out,
                }));
                let (statuses, agent_statuses) =
                    wait_final_event_statuses(&self.deps, &req.targets, &output);
                emit_collab_waiting_end(&self.deps, ctx, statuses, agent_statuses);
                self.deps
                    .emit("agent.wait.finished", wait_finished_payload(&output));
                return Ok(output);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !self.deps.manager.wait_any(remaining).await {
                let output = ok_output(json!({
                    "status": {},
                    "timed_out": true,
                }));
                emit_collab_waiting_end(&self.deps, ctx, serde_json::Map::new(), Vec::new());
                self.deps
                    .emit("agent.wait.finished", wait_finished_payload(&output));
                return Ok(output);
            }
        }
    }
}

// ----------------------------------------------------------------------------
// send_input
// ----------------------------------------------------------------------------

/// Wire args for `send_input`: deliver a message to a running child agent.
#[derive(Debug, Clone, Deserialize)]
pub struct SendInputRequest {
    pub target: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub items: Option<Vec<LegacyInputItem>>,
    #[serde(default)]
    pub interrupt: Option<bool>,
}

/// The `send_input` tool: enqueue an inter-agent communication onto the shared
/// mailbox (codex `enqueue_mailbox_communication`), waking the child.
pub struct SendInputTool {
    deps: SubagentToolDeps,
}

impl SendInputTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<SendInputRequest> for SendInputTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &SendInputRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for SendInputTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<SendInputRequest, ExecOutput> for SendInputTool {
    fn parallel_safe(&self, _req: &SendInputRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &SendInputRequest,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let input = legacy_input_payload(req.message.as_deref(), req.items.as_deref())?;
        let message = input.preview;
        let interrupt = req.interrupt.unwrap_or(false);
        if let Some(delivery) = store_message_tool_v1(
            &self.deps,
            &req.target,
            &message,
            input.input_items.clone(),
            interrupt,
        )? {
            let target = target_from_store_delivery(&delivery);
            emit_collab_interaction_begin(&self.deps, ctx, &target.thread_id, &message);
            let status = target_from_store_agent(&self.deps, &target.thread_id)
                .ok()
                .and_then(|target| target.status)
                .unwrap_or_else(|| Value::String("running".to_string()));
            emit_collab_interaction_end(&self.deps, ctx, &target, &message, status);
            self.deps.emit(
                "subagent.input",
                json!({
                    "agent_path": delivery.agent_path,
                    "agent_id": delivery.agent_id,
                    "message": message,
                    "trigger_turn": true,
                    "message_id": delivery.message_id,
                    "interrupt": interrupt,
                }),
            );
            return Ok(ok_output(json!({
                "submission_id": delivery.message_id,
            })));
        }
        let target_id = legacy_agent_id_target(&req.target)?;
        if interrupt {
            self.deps
                .manager
                .interrupt_agent_id(target_id)
                .map_err(|err| ToolError::Other(anyhow::anyhow!("send_input failed: {err}")))?;
        }
        let target = self
            .deps
            .manager
            .send_message_to_agent_id_with_items(
                &self.deps.parent,
                target_id,
                &message,
                input.input_items.clone(),
                true,
            )
            .map_err(|err| ToolError::Other(anyhow::anyhow!("send_input failed: {err}")))?;
        let target_event = target_from_record(&target);
        emit_collab_interaction_begin(&self.deps, ctx, &target_event.thread_id, &message);
        emit_collab_interaction_end(
            &self.deps,
            ctx,
            &target_event,
            &message,
            target_event
                .status
                .clone()
                .unwrap_or_else(|| Value::String("running".to_string())),
        );
        self.deps.emit(
            "subagent.input",
            json!({
                "agent_path": target.agent_path.clone(),
                "message": message,
                "trigger_turn": true,
                "interrupt": interrupt,
            }),
        );
        self.deps.emit(
            "agent.message",
            json!({
                "author_path": self.deps.parent.agent_path.clone(),
                "recipient_path": target.agent_path.clone(),
                "child_session_id": target.agent_id.clone(),
                "content": message,
                "input_items": input.input_items,
                "input_kind": "user_input",
                "trigger_turn": true,
                "interrupt": interrupt,
            }),
        );
        Ok(ok_output(json!({
            "submission_id": target.agent_id,
        })))
    }
}

// ----------------------------------------------------------------------------
// send_message / followup_task
// ----------------------------------------------------------------------------

/// Wire args for `send_message`: queue a message on a running agent without
/// triggering a fresh turn.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageRequest {
    pub target: String,
    pub message: String,
}

/// Wire args for `followup_task`: queue a message and trigger the target's next
/// turn. Root is rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowupTaskRequest {
    pub target: String,
    pub message: String,
}

pub struct SendMessageTool {
    deps: SubagentToolDeps,
}

impl SendMessageTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

pub struct FollowupTaskTool {
    deps: SubagentToolDeps,
}

impl FollowupTaskTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<SendMessageRequest> for SendMessageTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &SendMessageRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

#[async_trait]
impl Approvable<FollowupTaskRequest> for FollowupTaskTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &FollowupTaskRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for SendMessageTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

impl Sandboxable for FollowupTaskTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<SendMessageRequest, ExecOutput> for SendMessageTool {
    fn parallel_safe(&self, _req: &SendMessageRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &SendMessageRequest,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        run_agent_message_tool(
            &self.deps,
            ctx,
            &req.target,
            &req.message,
            false,
            "send_message",
        )
        .await
    }
}

#[async_trait]
impl ToolRuntime<FollowupTaskRequest, ExecOutput> for FollowupTaskTool {
    fn parallel_safe(&self, _req: &FollowupTaskRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &FollowupTaskRequest,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        run_agent_message_tool(
            &self.deps,
            ctx,
            &req.target,
            &req.message,
            true,
            "followup_task",
        )
        .await
    }
}

async fn run_agent_message_tool(
    deps: &SubagentToolDeps,
    ctx: &ToolCtx,
    target: &str,
    message: &str,
    trigger_turn: bool,
    tool_name: &str,
) -> Result<ExecOutput, ToolError> {
    if let Some(delivery) = store_message_tool(deps, target, message, trigger_turn, false)? {
        let target = target_from_store_delivery(&delivery);
        emit_collab_interaction_begin(deps, ctx, &target.thread_id, message);
        let status = target_from_store_agent(deps, &target.thread_id)
            .ok()
            .and_then(|target| target.status)
            .unwrap_or_else(|| Value::String("running".to_string()));
        emit_collab_interaction_end(deps, ctx, &target, message, status);
        deps.emit(
            "subagent.input",
            json!({
                "agent_path": delivery.agent_path,
                "agent_id": delivery.agent_id,
                "message": message,
                "trigger_turn": trigger_turn,
                "tool": tool_name,
                "message_id": delivery.message_id,
            }),
        );
        return Ok(empty_output());
    }
    let target = deps
        .manager
        .send_message_to_agent(&deps.parent, target, message, trigger_turn)
        .map_err(|err| ToolError::Other(anyhow::anyhow!("{tool_name} failed: {err}")))?;
    let target_event = target_from_record(&target);
    emit_collab_interaction_begin(deps, ctx, &target_event.thread_id, message);
    emit_collab_interaction_end(
        deps,
        ctx,
        &target_event,
        message,
        target_event
            .status
            .clone()
            .unwrap_or_else(|| Value::String("running".to_string())),
    );
    deps.emit(
        "subagent.input",
        json!({
            "agent_path": target.agent_path.clone(),
            "message": message,
            "trigger_turn": trigger_turn,
            "tool": tool_name,
        }),
    );
    deps.emit(
        "agent.message",
        json!({
            "author_path": deps.parent.agent_path.clone(),
            "recipient_path": target.agent_path.clone(),
            "child_session_id": target.agent_id.clone(),
            "content": message,
            "input_items": Value::Null,
            "input_kind": "inter_agent",
            "trigger_turn": trigger_turn,
            "tool": tool_name,
        }),
    );
    Ok(empty_output())
}

// ----------------------------------------------------------------------------
// resume_agent
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ResumeAgentRequest {
    pub id: String,
}

pub struct ResumeAgentTool {
    deps: SubagentToolDeps,
}

impl ResumeAgentTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<ResumeAgentRequest> for ResumeAgentTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &ResumeAgentRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for ResumeAgentTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<ResumeAgentRequest, ExecOutput> for ResumeAgentTool {
    fn parallel_safe(&self, _req: &ResumeAgentRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &ResumeAgentRequest,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let target_id = legacy_agent_id_target(&req.id)?;
        reject_legacy_depth_limit(&self.deps)?;
        let event_target = if self.deps.store.is_some() {
            target_from_store_agent(&self.deps, target_id).unwrap_or_else(|_| AgentEventTarget {
                thread_id: target_id.to_string(),
                nickname: None,
                role: None,
                status: Some(Value::String("not_found".to_string())),
            })
        } else {
            target_from_manager_agent_id(&self.deps, target_id).unwrap_or_else(|_| {
                AgentEventTarget {
                    thread_id: target_id.to_string(),
                    nickname: None,
                    role: None,
                    status: Some(Value::String("not_found".to_string())),
                }
            })
        };
        emit_collab_resume_begin(&self.deps, ctx, &event_target);
        if let Some(shared_store) = self.deps.store.as_ref() {
            let should_resume = {
                let store = shared_store
                    .lock()
                    .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
                let session = store
                    .load_session(target_id)
                    .map_err(|err| tool_err("load resumed child failed", err))?
                    .ok_or_else(|| {
                        ToolError::Other(anyhow::anyhow!("agent with id {target_id} not found"))
                    })?;
                if session.parent_id.is_none() {
                    return Err(ToolError::Other(anyhow::anyhow!(
                        "root is not a spawned agent"
                    )));
                }
                let summary = store
                    .agent_summary_for_child(target_id)
                    .map_err(|err| tool_err("load resumed child edge failed", err))?
                    .ok_or_else(|| {
                        ToolError::Other(anyhow::anyhow!(
                            "unknown child agent edge for session id: {target_id}"
                        ))
                    })?;
                let should_resume = resumable_child_state(session.status, &summary.status);
                if should_resume {
                    store
                        .reopen_child_agent_subtree(target_id)
                        .map_err(|err| tool_err("resume child agent failed", err))?;
                }
                should_resume
            };
            if should_resume {
                let requests = store_resume_requests_for_agent_subtree(shared_store, target_id)?;
                if let Some(runner) = self.deps.child_runner.as_ref() {
                    for request in requests {
                        runner
                            .run(request)
                            .map_err(|err| tool_err("resume child agent failed", err))?;
                    }
                }
            }
            let status = {
                let store = shared_store
                    .lock()
                    .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
                let session = store
                    .load_session(target_id)
                    .map_err(|err| tool_err("load resumed child failed", err))?
                    .ok_or_else(|| {
                        ToolError::Other(anyhow::anyhow!("agent with id {target_id} not found"))
                    })?;
                let summary = store
                    .agent_summary_for_child(target_id)
                    .map_err(|err| tool_err("load resumed child edge failed", err))?;
                local_agent_status_value(&store, &session, summary.as_ref())
                    .map_err(|err| tool_err("read resumed child status failed", err))?
            };
            let end_target = target_from_store_agent(&self.deps, target_id)
                .unwrap_or_else(|_| event_target.clone());
            emit_collab_resume_end(&self.deps, ctx, &end_target, status.clone());
            self.deps.emit(
                "agent.resumed",
                json!({
                    "child_session_id": target_id,
                    "status": status.clone(),
                    "resumed": should_resume,
                }),
            );
            return Ok(ok_output(json!({ "status": status })));
        }

        let status = self
            .deps
            .manager
            .registry()
            .list_agents()
            .into_iter()
            .find(|record| record.agent_id == target_id)
            .map(|record| record.status)
            .unwrap_or(AgentStatus::NotFound);
        let status_value = agent_status_value(&status);
        let end_target =
            target_from_manager_agent_id(&self.deps, target_id).unwrap_or(event_target);
        emit_collab_resume_end(&self.deps, ctx, &end_target, status_value.clone());
        self.deps.emit(
            "agent.resumed",
            json!({
                "child_session_id": target_id,
                "status": status_value.clone(),
                "resumed": false,
            }),
        );
        Ok(ok_output(json!({ "status": status_value })))
    }
}

// ----------------------------------------------------------------------------
// close_agent
// ----------------------------------------------------------------------------

/// Wire args for `close_agent`: close a child agent and descendants.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseAgentRequest {
    pub target: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloseAgentV1Request {
    pub target: String,
}

pub struct CloseAgentTool {
    deps: SubagentToolDeps,
    legacy_target_by_id: bool,
}

impl CloseAgentTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self {
            deps,
            legacy_target_by_id: false,
        }
    }

    pub fn new_legacy(deps: SubagentToolDeps) -> Self {
        Self {
            deps,
            legacy_target_by_id: true,
        }
    }

    fn event_target(&self, target: &str) -> Result<AgentEventTarget, ToolError> {
        if self.legacy_target_by_id {
            let target_id = legacy_agent_id_target(target)?;
            if self.deps.store.is_some() {
                return Ok(
                    target_from_store_agent(&self.deps, target_id).unwrap_or_else(|_| {
                        AgentEventTarget {
                            thread_id: target_id.to_string(),
                            nickname: None,
                            role: None,
                            status: Some(Value::String("not_found".to_string())),
                        }
                    }),
                );
            }
            return Ok(
                target_from_manager_agent_id(&self.deps, target_id).unwrap_or_else(|_| {
                    AgentEventTarget {
                        thread_id: target_id.to_string(),
                        nickname: None,
                        role: None,
                        status: Some(Value::String("not_found".to_string())),
                    }
                }),
            );
        }
        if self.deps.store.is_some() {
            target_from_store_reference_v2(&self.deps, target)
        } else {
            target_from_manager_reference_v2(&self.deps, target)
        }
    }

    async fn run_close(&self, target: &str, ctx: &ToolCtx) -> Result<ExecOutput, ToolError> {
        let event_target = self.event_target(target)?;
        emit_collab_close_begin(&self.deps, ctx, &event_target.thread_id);
        let store_output = if self.legacy_target_by_id {
            store_close_agent_v1(&self.deps, target)?
        } else {
            store_close_agent(&self.deps, target)?
        };
        if let Some(output) = store_output {
            emit_collab_close_end(
                &self.deps,
                ctx,
                &event_target,
                event_target
                    .status
                    .clone()
                    .unwrap_or_else(|| Value::String("not_found".to_string())),
            );
            self.deps.emit(
                "subagent.closed",
                json!({
                    "target": target,
                }),
            );
            return Ok(output);
        }
        let previous = if self.legacy_target_by_id {
            let target_id = legacy_agent_id_target(target)?;
            self.deps
                .manager
                .close_agent_id(target_id)
                .map_err(|err| ToolError::Other(anyhow::anyhow!("close_agent failed: {err}")))?
        } else {
            self.deps
                .manager
                .close_agent_reference(&self.deps.parent, target)
                .map_err(|err| ToolError::Other(anyhow::anyhow!("close_agent failed: {err}")))?
        };
        let previous_status = agent_status_value(&previous);
        emit_collab_close_end(&self.deps, ctx, &event_target, previous_status.clone());
        self.deps.emit(
            "subagent.closed",
            json!({
                "target": target,
                "previous_status": previous.wire_value(),
            }),
        );
        Ok(ok_output(json!({
            "previous_status": previous_status,
        })))
    }
}

#[async_trait]
impl Approvable<CloseAgentRequest> for CloseAgentTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &CloseAgentRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

#[async_trait]
impl Approvable<CloseAgentV1Request> for CloseAgentTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &CloseAgentV1Request) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for CloseAgentTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<CloseAgentRequest, ExecOutput> for CloseAgentTool {
    fn parallel_safe(&self, _req: &CloseAgentRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &CloseAgentRequest,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        self.run_close(&req.target, ctx).await
    }
}

#[async_trait]
impl ToolRuntime<CloseAgentV1Request, ExecOutput> for CloseAgentTool {
    fn parallel_safe(&self, _req: &CloseAgentV1Request) -> bool {
        false
    }

    async fn run(
        &self,
        req: &CloseAgentV1Request,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        self.run_close(&req.target, ctx).await
    }
}

// ----------------------------------------------------------------------------
// list_agents
// ----------------------------------------------------------------------------

/// Wire args for `list_agents` (no arguments).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListAgentsRequest {
    #[serde(default)]
    pub path_prefix: Option<String>,
}

/// The `list_agents` tool: a read-only snapshot of the live-agent registry.
pub struct ListAgentsTool {
    deps: SubagentToolDeps,
}

impl ListAgentsTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<ListAgentsRequest> for ListAgentsTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &ListAgentsRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for ListAgentsTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<ListAgentsRequest, ExecOutput> for ListAgentsTool {
    fn parallel_safe(&self, _req: &ListAgentsRequest) -> bool {
        false
    }

    async fn run(
        &self,
        _req: &ListAgentsRequest,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        if let Some(output) = store_list_agents(&self.deps, _req.path_prefix.as_deref())? {
            return Ok(output);
        }
        let agents: Vec<serde_json::Value> = self
            .deps
            .manager
            .list_agents_filtered(&self.deps.parent, _req.path_prefix.as_deref())
            .map_err(|err| ToolError::Other(anyhow::anyhow!("list_agents failed: {err}")))?
            .into_iter()
            .map(|record| {
                json!({
                    "agent_name": record.agent_path,
                    "agent_status": agent_status_value(&record.status),
                    "last_task_message": record.last_task_message,
                })
            })
            .collect();
        Ok(ok_output(json!({ "agents": agents })))
    }
}

/// Render a JSON body as a successful tool output (exit 0, body on stdout).
fn ok_output(body: serde_json::Value) -> ExecOutput {
    ExecOutput {
        exit_code: 0,
        stdout: body.to_string(),
        stderr: String::new(),
    }
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod subagent_tests;
