//! PURE rollback reducer (codex `rollback.rs:23-203`).
//!
//! Ported verbatim-in-behavior from `browser-use-core/src/rollback.rs`. The message /
//! event predicates it relies on live in `reconstruct.rs` (shared with the reducer).

use super::ProviderMessage;
use crate::session::reconstruct::{
    is_real_user_event, is_turn_aborted_message, is_user_message_for_rollback,
    COLLABORATION_CONTEXT_EVENT, COLLABORATION_CONTEXT_MESSAGE_NAME, GENERATED_IMAGE_CONTEXT_EVENT,
    GENERATED_IMAGE_CONTEXT_MESSAGE_NAME, MENTION_CONTEXT_MESSAGE_NAME, MODEL_SWITCH_CONTEXT_EVENT,
    MODEL_SWITCH_CONTEXT_MESSAGE_NAME, PERMISSIONS_CONTEXT_MESSAGE_NAME, PERSONALITY_CONTEXT_EVENT,
    PERSONALITY_CONTEXT_MESSAGE_NAME, SESSION_ROLLBACK_EVENT, WORKSPACE_CONTEXT_MESSAGE_NAME,
};
use browser_use_protocol::EventRecord;
use serde_json::Value;

pub fn rollback_filtered_events_after<'a>(
    events: &'a [EventRecord],
    after_seq: i64,
    messages: &mut Vec<ProviderMessage>,
) -> Vec<&'a EventRecord> {
    rollback_filtered_events_after_with_options(events, after_seq, messages, true)
}

pub fn rollback_filtered_events_after_for_fork<'a>(
    events: &'a [EventRecord],
    after_seq: i64,
    messages: &mut Vec<ProviderMessage>,
) -> Vec<&'a EventRecord> {
    rollback_filtered_events_after_with_options(events, after_seq, messages, true)
}

fn rollback_filtered_events_after_with_options<'a>(
    events: &'a [EventRecord],
    after_seq: i64,
    messages: &mut Vec<Value>,
    count_inter_agent_turns: bool,
) -> Vec<&'a EventRecord> {
    let mut replay_events = Vec::new();
    for event in events.iter().filter(|event| event.seq > after_seq) {
        if event.event_type == SESSION_ROLLBACK_EVENT {
            rollback_last_n_user_turns(
                &mut replay_events,
                messages,
                rollback_turn_count(&event.payload),
                count_inter_agent_turns,
            );
        } else {
            replay_events.push(event);
        }
    }
    replay_events
}

pub fn rollback_turn_count(payload: &Value) -> usize {
    match payload
        .get("num_turns")
        .or_else(|| payload.get("turns"))
        .or_else(|| payload.get("n"))
        .and_then(Value::as_u64)
    {
        Some(count) => usize::try_from(count).unwrap_or(usize::MAX),
        None => 1,
    }
}

fn rollback_last_n_user_turns(
    events: &mut Vec<&EventRecord>,
    checkpoint_messages: &mut Vec<Value>,
    mut count: usize,
    count_inter_agent_turns: bool,
) {
    while count > 0 {
        if rollback_last_user_event_turn(events, count_inter_agent_turns)
            || rollback_last_user_message_turn(checkpoint_messages, count_inter_agent_turns)
        {
            count -= 1;
        } else {
            break;
        }
    }
}

fn rollback_last_user_event_turn(
    events: &mut Vec<&EventRecord>,
    count_inter_agent_turns: bool,
) -> bool {
    let Some(user_pos) = events
        .iter()
        .rposition(|event| is_real_user_event_for_rollback(event, count_inter_agent_turns))
    else {
        return false;
    };
    let target_seq = events[user_pos].seq;
    let mut truncate_at = user_pos;
    while truncate_at > 0 && contextual_event_targets_turn(events[truncate_at - 1], target_seq) {
        truncate_at -= 1;
    }
    events.truncate(truncate_at);
    true
}

fn is_real_user_event_for_rollback(event: &EventRecord, count_inter_agent_turns: bool) -> bool {
    is_real_user_event(event)
        || (count_inter_agent_turns && agent_message_is_inter_agent_turn_event(event))
}

fn agent_message_is_inter_agent_turn_event(event: &EventRecord) -> bool {
    matches!(
        event.event_type.as_str(),
        "agent.message" | "agent.mailbox_input"
    ) && event
        .payload
        .get("content")
        .and_then(Value::as_str)
        .is_some()
}

fn contextual_event_targets_turn(event: &EventRecord, target_seq: i64) -> bool {
    matches!(
        event.event_type.as_str(),
        "workspace.context"
            | MODEL_SWITCH_CONTEXT_EVENT
            | PERSONALITY_CONTEXT_EVENT
            | COLLABORATION_CONTEXT_EVENT
            | GENERATED_IMAGE_CONTEXT_EVENT
    ) && event.payload.get("before_seq").and_then(Value::as_i64) == Some(target_seq)
}

fn rollback_last_user_message_turn(
    messages: &mut Vec<Value>,
    count_inter_agent_turns: bool,
) -> bool {
    let Some(user_pos) = messages
        .iter()
        .rposition(|message| is_user_message_for_rollback(message, count_inter_agent_turns))
    else {
        return false;
    };
    let has_prior_real_user = messages[..user_pos]
        .iter()
        .any(|message| is_user_message_for_rollback(message, count_inter_agent_turns));
    let mut truncate_at = user_pos;
    if has_prior_real_user {
        while truncate_at > 0 && is_contextual_provider_message(&messages[truncate_at - 1]) {
            truncate_at -= 1;
        }
    }
    messages.truncate(truncate_at);
    true
}

fn is_contextual_provider_message(message: &Value) -> bool {
    if is_turn_aborted_message(message) {
        return true;
    }
    matches!(
        message.get("name").and_then(Value::as_str),
        Some(WORKSPACE_CONTEXT_MESSAGE_NAME)
            | Some(PERMISSIONS_CONTEXT_MESSAGE_NAME)
            | Some(MODEL_SWITCH_CONTEXT_MESSAGE_NAME)
            | Some(PERSONALITY_CONTEXT_MESSAGE_NAME)
            | Some(COLLABORATION_CONTEXT_MESSAGE_NAME)
            | Some(MENTION_CONTEXT_MESSAGE_NAME)
            | Some(GENERATED_IMAGE_CONTEXT_MESSAGE_NAME)
    )
}
