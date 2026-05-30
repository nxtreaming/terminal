//! PURE rollback reducer (codex `rollback.rs:23-203`).

use super::ProviderMessage;
use browser_use_protocol::EventRecord;

pub fn rollback_filtered_events_after<'a>(
    _events: &'a [EventRecord],
    _after_seq: i64,
    _messages: &mut Vec<ProviderMessage>,
) -> Vec<&'a EventRecord> {
    unimplemented!()
}

pub fn rollback_filtered_events_after_for_fork<'a>(
    _events: &'a [EventRecord],
    _after_seq: i64,
    _messages: &mut Vec<ProviderMessage>,
) -> Vec<&'a EventRecord> {
    unimplemented!()
}

pub fn rollback_turn_count(_payload: &serde_json::Value) -> usize {
    unimplemented!()
}
