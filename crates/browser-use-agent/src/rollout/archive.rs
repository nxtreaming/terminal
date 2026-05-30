//! Archival of a (truncated/forked) rollout over the SQLite write-sink.
//!
//! Codex / legacy parity for the archival *shape*:
//! - legacy `browser-use-core/src/persistence.rs` records rollout/tool outputs
//!   into the `Store` by appending events through `store.append_event(...)`
//!   (e.g. `record_python_response_events_inner:206`); archival here mirrors that
//!   "serialize each item, append through the store" discipline.
//! - codex `rollout-trace/src/{bundle,writer}.rs`: a bundle is a durable artifact
//!   (`TraceBundleManifest` + an append-only `trace.jsonl` event log written by
//!   `TraceWriter::append`, writer.rs:108-134). We mirror the bundle = manifest
//!   (session id) + ordered event lines shape, persisted append-only.
//!
//! Write-sink discipline: this seam DUMPS the rollout for durability. It never
//! hot-reads the store during a turn (the engine reduces history from its
//! in-memory event log / `EventSource` replay path). The archived bundle is a
//! write-only durable record.
//!
//! `!Sync` safety: `browser_use_store::Store` wraps a rusqlite `Connection`
//! which is `Send + !Sync` (see `session/sink.rs:9`). The `Store`-backed archiver
//! therefore holds the store behind `Arc<Mutex<Store>>` and performs every SQLite
//! touch inside `tokio::task::spawn_blocking`, so the connection is only ever used
//! from a single blocking thread at a time and never shared across tokio tasks —
//! the same pattern as `session/sink.rs` (`Arc<Mutex<Store>>` + `spawn_blocking`).

use std::sync::{Arc, Mutex};

use browser_use_protocol::EventRecord;

/// A durable archived rollout bundle: the session it belongs to plus the
/// archived (truncated-off) and kept events.
///
/// Shape parity: codex `rollout-trace/src/bundle.rs` keys a bundle by its
/// thread/rollout id and stores its event lines; legacy persistence appends each
/// item under a `session_id`. Here a bundle carries both partitions so a later
/// reader can reconstruct the full pre-truncation ordering.
///
/// (`PartialEq` only — [`EventRecord`]'s `payload: serde_json::Value` is not
/// `Eq`.)
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutBundle {
    /// The session id this bundle belongs to.
    pub session_id: String,
    /// The archived (truncated-off) events, oldest first.
    pub archived_events: Vec<EventRecord>,
    /// The kept events that remain live.
    pub kept_events: Vec<EventRecord>,
}

impl RolloutBundle {
    /// Total number of events in the bundle (archived + kept).
    pub fn total(&self) -> usize {
        self.archived_events.len() + self.kept_events.len()
    }

    /// Whether this bundle archived anything.
    pub fn did_archive(&self) -> bool {
        !self.archived_events.is_empty()
    }
}

/// Errors produced while archiving a rollout.
///
/// Shape parity: mirrors `SessionRolloutError`-style init/IO mapping in
/// `codex-rs/core/src/session_rollout_init_error.rs` (a serialize failure vs an
/// archive/store failure).
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("rollout serialize failed: {0}")]
    Serialize(String),
    #[error("rollout archive failed: {0}")]
    Archive(String),
}

/// A sink that durably persists rollout bundles. Test fakes and the real
/// `Store`-backed archiver both implement this.
///
/// Shape parity: mirrors codex's write-only `TraceWriter` seam
/// (`rollout-trace/src/writer.rs`) and legacy "append through the store"
/// persistence. The seam is `async` because the production impl talks to a
/// `!Sync` SQLite store via `spawn_blocking`. Native RPITIT `async fn` in traits
/// is stable on Rust 1.95, so no `async-trait` crate is needed; the manager binds
/// implementors generically (not via `dyn`).
pub trait RolloutArchiver: Send + Sync {
    /// Persist a whole rollout bundle. Returns the number of events written.
    ///
    /// Write-only: implementations must not depend on hot-reading the store.
    fn archive_rollout(
        &self,
        bundle: &RolloutBundle,
    ) -> impl std::future::Future<Output = Result<usize, ArchiveError>> + Send;
}

/// The event type under which an archived rollout event is durably recorded.
///
/// Archived events are dumped as new rows of this type so they never collide
/// with (or get replayed as) the live `STORE_EVENT_TYPES` the reducer consumes —
/// keeping the write-sink-only, never-hot-read contract.
pub const ROLLOUT_ARCHIVE_EVENT_TYPE: &str = "rollout.archived";

/// Serialize a bundle's events into ordered archive payloads.
///
/// Parity: archived lines first, then kept (codex `TraceWriter` appends in
/// order; legacy persistence chains its records), so a later reader can
/// reconstruct the original ordering. Each payload records the original seq,
/// partition, and the verbatim event for durability.
fn bundle_payloads(bundle: &RolloutBundle) -> Result<Vec<serde_json::Value>, ArchiveError> {
    let mut payloads = Vec::with_capacity(bundle.total());
    let mut push = |event: &EventRecord, partition: &str| -> Result<(), ArchiveError> {
        let event_json =
            serde_json::to_value(event).map_err(|e| ArchiveError::Serialize(e.to_string()))?;
        payloads.push(serde_json::json!({
            "partition": partition,
            "orig_seq": event.seq,
            "event": event_json,
        }));
        Ok(())
    };
    for event in &bundle.archived_events {
        push(event, "archived")?;
    }
    for event in &bundle.kept_events {
        push(event, "kept")?;
    }
    Ok(payloads)
}

/// A real archiver backed by `browser_use_store::Store`.
///
/// The store is held behind `Arc<Mutex<Store>>`; every SQLite call runs inside
/// `spawn_blocking`, never sharing the `!Sync` connection across tasks (same
/// pattern as `session/sink.rs`).
#[derive(Clone)]
pub struct StoreRolloutArchiver {
    store: Arc<Mutex<browser_use_store::Store>>,
}

impl StoreRolloutArchiver {
    /// Wrap an owned store for archival.
    pub fn new(store: browser_use_store::Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
        }
    }

    /// Wrap an already-shared store (e.g. the `SharedStore` from `session/sink`).
    pub fn from_shared(store: Arc<Mutex<browser_use_store::Store>>) -> Self {
        Self { store }
    }
}

impl RolloutArchiver for StoreRolloutArchiver {
    async fn archive_rollout(&self, bundle: &RolloutBundle) -> Result<usize, ArchiveError> {
        let session_id = bundle.session_id.clone();
        let payloads = bundle_payloads(bundle)?;
        let store = Arc::clone(&self.store);
        // !Sync-safe: SQLite is touched only inside spawn_blocking, under the
        // Mutex, on a single blocking thread.
        let written = tokio::task::spawn_blocking(move || -> Result<usize, ArchiveError> {
            let guard = store
                .lock()
                .map_err(|e| ArchiveError::Archive(format!("store mutex poisoned: {e}")))?;
            let mut count = 0usize;
            for payload in payloads {
                guard
                    .append_event(&session_id, ROLLOUT_ARCHIVE_EVENT_TYPE, payload)
                    .map_err(|e| ArchiveError::Archive(e.to_string()))?;
                count += 1;
            }
            Ok(count)
        })
        .await
        .map_err(|e| ArchiveError::Archive(format!("archive task join failed: {e}")))??;
        Ok(written)
    }
}
