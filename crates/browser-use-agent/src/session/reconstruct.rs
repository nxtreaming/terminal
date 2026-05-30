//! PURE event reducer (codex `lib.rs:8863-9646`). Rebuilds provider history from events.

use super::ProviderMessage;
use browser_use_protocol::EventRecord;

pub fn provider_messages_from_events(_events: &[EventRecord]) -> Vec<ProviderMessage> {
    unimplemented!()
}

pub fn provider_messages_from_events_for_fork(_events: &[EventRecord]) -> Vec<ProviderMessage> {
    unimplemented!()
}

pub struct CompactionReplayState {
    pub seq: i64,
    pub messages: Vec<ProviderMessage>,
    pub initial_context_already_in_history: bool,
}

pub fn latest_compaction_replacement_history(
    _events: &[EventRecord],
) -> Option<CompactionReplayState> {
    unimplemented!()
}

pub fn provider_messages_from_event_slice(
    _events: &[&EventRecord],
    _messages: &mut Vec<ProviderMessage>,
    _has_checkpoint: bool,
    _include_agent_messages: bool,
) -> Vec<ProviderMessage> {
    unimplemented!()
}

pub fn initial_context_messages_from_events(
    _events: &[EventRecord],
    _first_user_seq: Option<i64>,
    _include_all_anchored: bool,
    _inject_default_permissions: bool,
) -> Vec<ProviderMessage> {
    unimplemented!()
}

pub fn is_real_user_event(_e: &EventRecord) -> bool {
    unimplemented!()
}

pub fn provider_history_has_open_turn(_messages: &[ProviderMessage]) -> bool {
    unimplemented!()
}

/// `policy.rs:17-221` mapped.
pub fn is_persistable_event(_ty: &str, _payload: &serde_json::Value) -> bool {
    unimplemented!()
}
