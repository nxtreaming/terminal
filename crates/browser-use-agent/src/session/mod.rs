//! `session/` — lifecycle, resume-by-replay, fork/rollback (PURE reducer + thin async).
//!
//! Reconciliation (WP-A0): `browser_use_protocol::EventRecord` already exists in the
//! protocol crate (`lib.rs:57`), so the reducer consumes the real type directly — no
//! local `EventRecord` definition is needed. `browser_use_store::StoreNotification`
//! also exists, so `EventNotifier::subscribe` keeps the sketch's payload type.

pub mod notifier;
pub mod reconstruct;
#[cfg(test)]
mod reconstruct_tests;
pub mod resume;
pub mod rollback;
pub mod sink;

/// Legacy currency = `serde_json::Value` (== `context::Item`).
pub use crate::context::Item as ProviderMessage;

/// Re-export so downstream WPs can name the reduced-from event type.
pub use browser_use_protocol::EventRecord;
pub use browser_use_protocol::EventRecord as ReducerEvent;

pub use notifier::{wait_for_events_after_seq, EventNotifier};
pub use reconstruct::{
    initial_context_messages_from_events, is_persistable_event, is_real_user_event,
    latest_compaction_replacement_history, provider_history_has_open_turn,
    provider_messages_from_event_slice, provider_messages_from_events,
    provider_messages_from_events_for_fork, CompactionReplayState,
};
pub use rollback::{
    rollback_filtered_events_after, rollback_filtered_events_after_for_fork, rollback_turn_count,
};
pub use sink::{EventSeq, EventSink, EventSource, SessionId};

// ---- async lifecycle (thin: read once -> pure reduce -> install) ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

pub enum ForkMode {
    None,
    LastN(usize),
    All,
    Summary,
}

pub struct Session {
    // id, history: Vec<ProviderMessage>, sink, status, last_seq
}

impl Session {
    pub async fn create(
        _parent: Option<SessionId>,
        _cwd: std::path::PathBuf,
        _sink: std::sync::Arc<dyn EventSink>,
    ) -> anyhow::Result<Self> {
        unimplemented!()
    }

    pub async fn resume(
        _id: SessionId,
        _src: std::sync::Arc<dyn EventSource>,
        _sink: std::sync::Arc<dyn EventSink>,
    ) -> anyhow::Result<Self> {
        unimplemented!()
    }

    pub async fn fork(
        &self,
        _src: std::sync::Arc<dyn EventSource>,
        _sink: std::sync::Arc<dyn EventSink>,
        _mode: ForkMode,
    ) -> anyhow::Result<Self> {
        unimplemented!()
    }

    pub async fn rollback(
        &mut self,
        _num_turns: usize,
        _src: std::sync::Arc<dyn EventSource>,
    ) -> anyhow::Result<EventSeq> {
        unimplemented!()
    }

    pub fn history(&self) -> &[ProviderMessage] {
        unimplemented!()
    }

    pub fn status(&self) -> SessionStatus {
        unimplemented!()
    }
}
