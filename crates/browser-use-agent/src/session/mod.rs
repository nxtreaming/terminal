//! `session/` — lifecycle, resume-by-replay, fork/rollback (PURE reducer + thin async).
//!
//! Reconciliation (WP-A0): `browser_use_protocol::EventRecord` already exists in the
//! protocol crate (`lib.rs:57`), so the reducer consumes the real type directly — no
//! local `EventRecord` definition is needed. `browser_use_store::StoreNotification`
//! also exists, so `EventNotifier::subscribe` keeps the sketch's payload type.
//!
//! WP-B4 fills in the async lifecycle. The pure cores (`reconstruct.rs` reducer +
//! `rollback.rs` filter, merged in A6) are read-only here; this module composes them with
//! the SQLite-backed `EventSink` / `EventSource` (`sink.rs`) and the event-notify
//! `EventNotifier` (`notifier.rs`). In-memory `history` is authoritative for the live turn
//! loop; SQLite is the durable log, read only at resume / fork / rollback.

pub mod notifier;
pub mod reconstruct;
#[cfg(test)]
mod reconstruct_tests;
pub mod resume;
pub mod rollback;
pub mod sink;

#[cfg(test)]
mod lifecycle_tests;

/// Legacy currency = `serde_json::Value` (== `context::Item`).
pub use crate::context::Item as ProviderMessage;

/// Re-export so downstream WPs can name the reduced-from event type.
pub use browser_use_protocol::EventRecord;
pub use browser_use_protocol::EventRecord as ReducerEvent;

pub use notifier::{
    spawn_notifier_bridge, wait_for_events_after_seq, EventNotifier, StoreEventNotifier,
};
pub use reconstruct::{
    initial_context_messages_from_events, is_persistable_event, is_real_user_event,
    latest_compaction_replacement_history, provider_history_has_open_turn,
    provider_messages_from_event_slice, provider_messages_from_events,
    provider_messages_from_events_for_fork, CompactionReplayState,
};
pub use rollback::{
    rollback_filtered_events_after, rollback_filtered_events_after_for_fork, rollback_turn_count,
};
pub use sink::{
    EventSeq, EventSink, EventSource, SessionId, SharedStore, StoreEventSink, StoreEventSource,
};

use std::sync::Arc;

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

/// Live session handle. `history` (in-memory provider messages) is authoritative for the
/// running turn loop; the durable event log behind `sink` is replayed only at resume /
/// fork / rollback.
pub struct Session {
    id: SessionId,
    history: Vec<ProviderMessage>,
    sink: Arc<dyn EventSink>,
    status: SessionStatus,
    /// Highest event seq this session has observed (0 for a fresh session).
    last_seq: EventSeq,
}

impl Session {
    /// Create a brand-new session with an empty in-memory history.
    ///
    /// The real `browser_use_store::Store` mints session ids itself, so the durable row is
    /// created here through the sink and the store-assigned id is adopted:
    /// - child (`parent` = Some): via the frozen `EventSink::create_child_session`.
    /// - root  (`parent` = None): via `EventSink::create_root_session` (a one-time setup
    ///   write the `StoreEventSink` backs against `Store::create_session`).
    pub async fn create(
        parent: Option<SessionId>,
        cwd: std::path::PathBuf,
        sink: Arc<dyn EventSink>,
    ) -> anyhow::Result<Self> {
        let id = match parent {
            Some(parent) => sink.create_child_session(&parent, cwd, None).await?,
            None => sink.create_root_session(cwd).await?,
        };

        Ok(Self {
            id,
            history: Vec::new(),
            sink,
            status: SessionStatus::Running,
            last_seq: 0,
        })
    }

    /// Resume by replaying the durable event log into provider history (codex
    /// `run_existing_session`): read all events -> pure reduce -> install as live history.
    pub async fn resume(
        id: SessionId,
        src: Arc<dyn EventSource>,
        sink: Arc<dyn EventSink>,
    ) -> anyhow::Result<Self> {
        let events = src.events_for_session(&id).await?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or(0);
        let history = resume::history_from_events(&events);
        Ok(Self {
            id,
            history,
            sink,
            status: SessionStatus::Running,
            last_seq,
        })
    }

    /// Fork: branch a child session from this session's (optionally truncated) history.
    ///
    /// Reconstructs the parent history with the fork-aware reducer, truncates per
    /// [`ForkMode`], creates a child session row (store-assigned id), and seeds the child
    /// with the carried history as an `agent.context` event so the child can itself be
    /// resumed (parity: legacy `fork_response_items`).
    pub async fn fork(
        &self,
        src: Arc<dyn EventSource>,
        sink: Arc<dyn EventSink>,
        mode: ForkMode,
    ) -> anyhow::Result<Self> {
        let events = src.events_for_session(&self.id).await?;
        let history = resume::fork_history_from_events(&events, &mode);

        let child_id = sink
            .create_child_session(&self.id, std::path::PathBuf::from("."), None)
            .await?;

        // Seed the child's durable log with the carried history so a later resume of the
        // child reconstructs the same starting point. The reducer expands an `agent.context`
        // event's `fork_response_items` via `response_items_to_provider_messages`, so the
        // carried provider messages are converted to response-item shape first (parity:
        // legacy `fork_response_items_for_spawn`).
        let mut last_seq = 0;
        if !history.is_empty() {
            let response_items = resume::provider_messages_to_response_items(&history);
            let payload = serde_json::json!({
                "history_mode": "fork_response_items",
                "fork_response_items": response_items,
            });
            let record = sink
                .append_event(&child_id, "agent.context", &payload)
                .await?;
            last_seq = record.seq;
        }

        Ok(Self {
            id: child_id,
            history,
            sink,
            status: SessionStatus::Running,
            last_seq,
        })
    }

    /// Roll back the last `num_turns` real user turns (canonical mechanism): append a durable
    /// `session.rollback` event carrying `num_turns`, then re-read the full log and reduce —
    /// `provider_messages_from_events` applies the rollback inline (the reducer's
    /// `rollback_filtered_events_after` drops the prior N user turns when it hits the
    /// `session.rollback` event). The truncated history is re-installed in memory and the
    /// informational rolled-back-to seq is returned.
    pub async fn rollback(
        &mut self,
        num_turns: usize,
        src: Arc<dyn EventSource>,
    ) -> anyhow::Result<EventSeq> {
        let pre_events = src.events_for_session(&self.id).await?;
        let after_seq = resume::rollback_after_seq_for_turns(&pre_events, num_turns);

        let payload = serde_json::json!({ "after_seq": after_seq, "num_turns": num_turns });
        let record = self
            .sink
            .append_event(&self.id, "session.rollback", &payload)
            .await?;

        // Re-read including the just-appended rollback event, then reduce.
        let events = src.events_for_session(&self.id).await?;
        self.history = resume::history_after_rollback(&events);
        self.last_seq = record.seq;
        Ok(after_seq)
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn last_seq(&self) -> EventSeq {
        self.last_seq
    }

    pub fn history(&self) -> &[ProviderMessage] {
        &self.history
    }

    pub fn status(&self) -> SessionStatus {
        self.status
    }

    pub fn set_status(&mut self, status: SessionStatus) {
        self.status = status;
    }
}
