//! `session/resume.rs` — resume-by-replay / fork / rollback glue (read events -> reduce -> install).
//!
//! Thin helpers; the heavy lifting is the pure reducer in `reconstruct.rs` and the pure
//! filtering in `rollback.rs`. These functions are pure (no I/O): `Session` in `mod.rs`
//! does the async store reads, then hands the owned event log here to rebuild history.

use super::reconstruct::provider_messages_from_events;
use super::{ForkMode, ProviderMessage};
use browser_use_protocol::EventRecord;
use serde_json::Value;

/// Resume: replay the durable event log into provider history (codex `run_existing_session`).
pub fn history_from_events(events: &[EventRecord]) -> Vec<ProviderMessage> {
    provider_messages_from_events(events)
}

/// Fork: rebuild the parent's provider history (fork variant keeps inter-agent turns),
/// then truncate per [`ForkMode`].
///
/// Mirrors Codex `truncate_rollout_to_last_n_fork_turns`: `None` carries no
/// history, `All` carries the full reconstructed history, and `LastN` keeps the
/// last N fork-turn boundaries, not the last N provider messages. A fork turn is
/// a real user turn or an inter-agent message with `trigger_turn = true`.
pub fn fork_history_from_events(events: &[EventRecord], mode: &ForkMode) -> Vec<ProviderMessage> {
    match mode {
        ForkMode::None => Vec::new(),
        ForkMode::All | ForkMode::Summary => {
            super::reconstruct::provider_messages_from_events_for_fork(events)
        }
        ForkMode::LastN(n) => {
            let effective = effective_events_after_rollbacks(events);
            let Some(start_seq) = last_n_fork_turns_start_seq(&effective, *n) else {
                return if *n > 0 {
                    super::reconstruct::provider_messages_from_events_for_fork(&effective)
                } else {
                    Vec::new()
                };
            };
            let carried = effective
                .iter()
                .filter(|event| event.seq >= start_seq)
                .cloned()
                .collect::<Vec<_>>();
            super::reconstruct::provider_messages_from_events_for_fork(&carried)
        }
    }
}

fn effective_events_after_rollbacks(events: &[EventRecord]) -> Vec<EventRecord> {
    let mut checkpoint_messages = Vec::new();
    super::rollback_filtered_events_after_for_fork(events, 0, &mut checkpoint_messages)
        .into_iter()
        .cloned()
        .collect()
}

fn last_n_fork_turns_start_seq(events: &[EventRecord], n: usize) -> Option<i64> {
    if n == 0 {
        return None;
    }
    let mut seqs = events
        .iter()
        .filter(|event| is_fork_turn_boundary_event(event))
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    seqs.sort_unstable();
    seqs.dedup();
    if seqs.is_empty() {
        return None;
    }
    if n >= seqs.len() {
        return Some(0);
    }
    Some(seqs[seqs.len() - n])
}

fn is_fork_turn_boundary_event(event: &EventRecord) -> bool {
    if super::reconstruct::is_real_user_event(event) {
        return true;
    }
    matches!(
        event.event_type.as_str(),
        "agent.message" | "agent.mailbox_input"
    ) && event
        .payload
        .get("content")
        .and_then(Value::as_str)
        .is_some()
        && event
            .payload
            .get("trigger_turn")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// Rollback (canonical mechanism): the caller has already appended a `session.rollback`
/// event carrying `num_turns` to the durable log. Reconstructing provider history over the
/// full log then applies the rollback inline — `provider_messages_from_events` ->
/// `rollback::rollback_filtered_events_after`, which on encountering the `session.rollback`
/// event drops the prior N real user turns from the replayed events. So rollback is just a
/// normal reduce over the post-rollback-event log.
pub fn history_after_rollback(events_including_rollback: &[EventRecord]) -> Vec<ProviderMessage> {
    provider_messages_from_events(events_including_rollback)
}

/// The seq the rollback "rolls back to": the seq just before the Nth-from-last real user
/// turn (`session.input` / `session.followup`) in the pre-rollback log. `0` means the whole
/// log was rolled back. This is informational (recorded in the `session.rollback` payload
/// and returned to the caller); the actual truncation is done by the reducer above.
pub fn rollback_after_seq_for_turns(events: &[EventRecord], num_turns: usize) -> i64 {
    if num_turns == 0 {
        return 0;
    }
    let mut user_turn_seqs: Vec<i64> = events
        .iter()
        .filter(|event| is_real_user_turn_event(event))
        .map(|event| event.seq)
        .collect();
    user_turn_seqs.sort_unstable();
    user_turn_seqs.dedup();
    if user_turn_seqs.is_empty() {
        return events.last().map(|event| event.seq).unwrap_or(0);
    }
    let index = user_turn_seqs.len().saturating_sub(num_turns);
    if index == 0 {
        return 0;
    }
    user_turn_seqs[index].saturating_sub(1)
}

/// A "real user turn" event for rollback counting: `session.input` / `session.followup`, or
/// an inter-agent `agent.message` / `agent.mailbox_input` carrying content (mirrors the
/// rollback reducer's `is_real_user_event_for_rollback`).
fn is_real_user_turn_event(event: &EventRecord) -> bool {
    super::reconstruct::is_real_user_event(event)
        || (matches!(
            event.event_type.as_str(),
            "agent.message" | "agent.mailbox_input"
        ) && event
            .payload
            .get("content")
            .and_then(Value::as_str)
            .is_some())
}

/// Convert reconstructed provider messages back into response-item shape so they can be
/// stored as `fork_response_items` and faithfully re-expanded on resume by the reducer's
/// `response_items_to_provider_messages`. Mirrors legacy `fork_response_items_for_spawn`,
/// which round-trips provider messages -> response items before persisting them.
///
/// Covers the shapes the reducer produces: user/assistant/developer/system text messages,
/// assistant tool calls, and tool outputs.
pub fn provider_messages_to_response_items(messages: &[ProviderMessage]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        // Tool output -> function_call_output.
        if role == "tool" {
            if let Some(call_id) = message.get("tool_call_id").and_then(Value::as_str) {
                items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": message_tool_output_for_response_item(message),
                }));
            }
            continue;
        }
        // Assistant tool calls -> one function_call item each.
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let Some(call_id) = call
                    .get("id")
                    .or_else(|| call.get("call_id"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
                let arguments = call
                    .get("arguments")
                    .map(|args| match args {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| "{}".to_string());
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
        }
        // Content -> message item (skip empty, e.g. tool-call-only assistant turns).
        let content = message_content_parts_for_response_item(message, role);
        if !content.is_empty() {
            items.push(serde_json::json!({
                "type": "message",
                "role": role,
                "content": content,
            }));
        }
    }
    items
}

/// Convert provider messages into the stricter Codex MultiAgentV2 fork-history
/// shape. Full-history forks keep system/developer/user messages and assistant
/// final-answer text, but drop reasoning, tool calls, and tool outputs.
pub fn provider_messages_to_fork_response_items(messages: &[ProviderMessage]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        if message.get("name").and_then(Value::as_str)
            == Some(super::reconstruct::MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME)
        {
            continue;
        }
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "system" | "developer" | "user" => {
                let content = message_content_parts_for_response_item(message, role);
                if !content.is_empty() {
                    items.push(serde_json::json!({
                        "type": "message",
                        "role": role,
                        "content": content,
                    }));
                }
            }
            "assistant" => {
                if message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
                {
                    continue;
                }
                let content = message_content_parts_for_response_item(message, role);
                if !content.is_empty() {
                    items.push(serde_json::json!({
                        "type": "message",
                        "role": role,
                        "content": content,
                    }));
                }
            }
            _ => {}
        }
    }
    items
}

fn message_tool_output_for_response_item(message: &Value) -> Value {
    let content = message_content_parts_for_response_item(message, "user");
    if content
        .iter()
        .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
    {
        Value::Array(content)
    } else {
        Value::String(message_text(message))
    }
}

fn message_content_parts_for_response_item(message: &Value, role: &str) -> Vec<Value> {
    let part_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![serde_json::json!({ "type": part_type, "text": text })]
        }
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| response_content_part(part, part_type))
            .collect(),
        _ => Vec::new(),
    }
}

fn response_content_part(part: &Value, text_type: &str) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_image") | Some("image") | Some("image_url") | Some("output_image") => {
            let image_url = part_image_url(part)?;
            let mut out = serde_json::json!({
                "type": "input_image",
                "image_url": image_url,
            });
            if let Some(detail) = part.get("detail").and_then(Value::as_str) {
                out["detail"] = Value::String(detail.to_string());
            }
            Some(out)
        }
        Some("input_text") | Some("output_text") | Some("text") | None => part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| serde_json::json!({ "type": text_type, "text": text })),
        _ => None,
    }
}

fn part_image_url(part: &Value) -> Option<String> {
    part.get("image_url")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("url").and_then(Value::as_str))
        })
        .or_else(|| part.get("url").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .or_else(|| {
            let data = part.get("data").and_then(Value::as_str)?;
            let mime_type = part
                .get("mime_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            Some(format!("data:{mime_type};base64,{data}"))
        })
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fork_response_items_keep_messages_and_drop_tool_rollout_items() {
        let messages = vec![
            json!({"role": "system", "content": "system guidance"}),
            json!({"role": "developer", "content": "developer guidance"}),
            json!({
                "role": "developer",
                "name": "multi_agent_usage_hint",
                "content": [{ "type": "input_text", "text": "parent-only hint" }]
            }),
            json!({"role": "user", "content": "do work"}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "name": "shell",
                    "arguments": "{}"
                }]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "tool output"}),
            json!({"role": "assistant", "content": "final answer"}),
        ];

        let items = provider_messages_to_fork_response_items(&messages);

        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["role"], json!("system"));
        assert_eq!(items[1]["role"], json!("developer"));
        assert_ne!(items[1]["content"][0]["text"], json!("parent-only hint"));
        assert_eq!(items[2]["role"], json!("user"));
        assert_eq!(items[3]["role"], json!("assistant"));
        assert_eq!(items[3]["content"][0]["text"], json!("final answer"));
        assert!(
            items
                .iter()
                .all(|item| item.get("type").and_then(Value::as_str) == Some("message")),
            "forked history must not carry tool calls or tool outputs: {items:?}"
        );
    }
}
