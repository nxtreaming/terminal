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
/// Mirrors legacy `fork_response_items` / `fork_turns`: `None` carries no history, `All`
/// carries the full reconstructed history, `LastN` keeps the last N provider messages, and
/// `Summary` is treated as `All` until a summary checkpoint is wired in (the reconstructed
/// history already honours any `session.compacted` replacement checkpoint, so a summarised
/// parent forks from its summary automatically).
pub fn fork_history_from_events(events: &[EventRecord], mode: &ForkMode) -> Vec<ProviderMessage> {
    let full = super::reconstruct::provider_messages_from_events_for_fork(events);
    match mode {
        ForkMode::None => Vec::new(),
        ForkMode::All | ForkMode::Summary => full,
        ForkMode::LastN(n) => {
            let n = (*n).min(full.len());
            full[full.len() - n..].to_vec()
        }
    }
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
                    "output": message_text(message),
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
        // Text content -> message item (skip empty, e.g. tool-call-only assistant turns).
        let text = message_text(message);
        if !text.is_empty() {
            let part_type = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            items.push(serde_json::json!({
                "type": "message",
                "role": role,
                "content": [{ "type": part_type, "text": text }],
            }));
        }
    }
    items
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
