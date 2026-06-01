//! Rollout hardening: size-bounded archival/truncation over the SQLite
//! write-sink, plus by-turn-boundary forking.
//!
//! Codex parity: a rollout is the durable, append-only thread log
//! ([`EventRecord`]s in this engine). To keep replay/fork bounded, codex
//! truncates the rollout once it exceeds a byte budget
//! (`conversation_history.rs:44-76`, 5 MiB), forks by turn boundary
//! (`thread_rollout_truncation.rs`), and archives durable artifacts
//! (`rollout-trace/{bundle,writer}.rs`). This module mirrors that triad:
//!
//! - [`truncation`] — pure size-bounded drop-oldest-until-fit (5 MiB default).
//! - [`fork`] — by-fork-turn fork that keeps real user turns plus triggered
//!   inter-agent turns (`LastN` is now per-turn; `Summary` is distinct from `All`).
//! - [`archive`] — async archival seam over the `!Sync` SQLite store.
//!
//! [`RolloutManager`] wires the three together: truncate a live log, build a
//! durable bundle from the (archived prefix + kept) slices, and dump that bundle
//! through a [`RolloutArchiver`].
//!
//! Cutover note: at engine cutover, `RolloutManager` is intended to be invoked
//! from the session lifecycle (`session/mod.rs`) right after a turn's events are
//! appended — truncate the in-memory/log view to the byte budget and archive the
//! dropped prefix. The session fork path also applies this by-turn boundary when
//! reconstructing fork history.

pub mod archive;
pub mod fork;
pub mod truncation;

pub use archive::{
    ArchiveError, RolloutArchiver, RolloutBundle, StoreRolloutArchiver, ROLLOUT_ARCHIVE_EVENT_TYPE,
};
pub use fork::{fork_events_by_turn, ForkOutcome, SummaryPlaceholder};
pub use truncation::{
    truncate_rollout_if_needed, TruncationOutcome, DEFAULT_THREAD_ROLLOUT_MAX_BYTES,
};

use browser_use_protocol::EventRecord;

use crate::session::ForkMode;

/// Ties truncation + fork + archive into one small façade.
///
/// Generic over the [`RolloutArchiver`] so tests inject a fake and production
/// uses [`StoreRolloutArchiver`]. The manager is pure-plus-one-await: it computes
/// the truncation/fork outcomes synchronously and only awaits the (write-only)
/// archive seam.
pub struct RolloutManager<A: RolloutArchiver> {
    archiver: A,
    max_bytes: usize,
}

impl<A: RolloutArchiver> RolloutManager<A> {
    /// Build a manager with the codex-default 5 MiB byte budget.
    pub fn new(archiver: A) -> Self {
        Self {
            archiver,
            max_bytes: DEFAULT_THREAD_ROLLOUT_MAX_BYTES,
        }
    }

    /// Build a manager with a custom byte budget (mainly for tests).
    pub fn with_max_bytes(archiver: A, max_bytes: usize) -> Self {
        Self {
            archiver,
            max_bytes,
        }
    }

    /// The configured byte budget.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Truncate a live event log to the configured budget without archiving.
    pub fn truncate(&self, events: &[EventRecord]) -> TruncationOutcome {
        truncate_rollout_if_needed(events, self.max_bytes)
    }

    /// Fork an event log by turn boundary per `mode`.
    pub fn fork(&self, events: &[EventRecord], mode: &ForkMode) -> ForkOutcome {
        fork_events_by_turn(events, mode)
    }

    /// Truncate a live log and archive the truncated-off prefix (plus the kept
    /// slice) as a durable bundle for `session_id`. Returns the truncation
    /// outcome and the number of events written to the sink.
    ///
    /// Write-sink discipline: this dumps for durability; it never hot-reads.
    pub async fn truncate_and_archive(
        &self,
        session_id: &str,
        events: &[EventRecord],
    ) -> Result<(TruncationOutcome, usize), ArchiveError> {
        let outcome = self.truncate(events);
        let bundle = RolloutBundle {
            session_id: session_id.to_string(),
            archived_events: outcome.archived.clone(),
            kept_events: outcome.kept.clone(),
        };
        let written = self.archiver.archive_rollout(&bundle).await?;
        Ok((outcome, written))
    }
}

#[cfg(test)]
mod tests;
