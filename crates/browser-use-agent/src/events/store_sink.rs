//! `store_sink.rs` — async writer task; the ONLY `EventSink` that touches the DB.
//!
//! `StoreSink` persists the canonical event log to SQLite. It is intentionally
//! **write-only**: at runtime it never reads back from the store. Reads are the
//! job of the replay/history layer, which queries the store directly.
//!
//! ## Async write pipeline
//!
//! The turn loop calls `emit` synchronously and must never block on disk I/O,
//! so `StoreSink` is an asynchronous writer:
//!
//! - [`StoreSink::spawn`] takes ownership of the [`Store`] and starts a dedicated
//!   OS thread that owns it and drains a [`std::sync::mpsc`] queue of
//!   [`StoreOp`] messages. A thin Tokio task joins that thread, yielding the
//!   frozen `JoinHandle<()>`.
//! - [`StoreSink::emit`] (the [`EventSink`] impl) enqueues a [`StoreOp::Event`]
//!   and returns immediately — fire-and-forget, never blocks the caller.
//! - [`StoreSink::emit_and_record`] enqueues a [`StoreOp::EventThenArtifact`] so
//!   the writer thread can append the event, learn its sequence number, and then
//!   record the artifact anchored to that exact seq.
//!
//! A dedicated std thread (rather than `tokio::task::spawn_blocking`) is required
//! because [`Store`] holds a rusqlite `Connection`, which is `Send` but not
//! `Sync`; `Arc<Store>` is therefore not `Send` and cannot be shared across the
//! Tokio worker pool. Owning the `Store` on exactly one thread sidesteps this
//! while keeping the synchronous rusqlite calls off the async runtime.
//!
//! Ops are drained in FIFO order and applied one at a time, which keeps event
//! sequence numbers monotonic and mirrors the legacy persistence ordering (event
//! first, then the seq-anchored artifact — see
//! `browser-use-core/src/persistence.rs`).
//!
//! Persistence is best-effort: store errors are swallowed rather than
//! propagated, because `emit`/`emit_and_record` are infallible fan-out hooks the
//! turn loop calls without `.await`. (The crate has no logging facade wired up
//! yet; a `tracing` hook can be added here once one exists.)

use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use browser_use_store::Store;
use tokio::task::JoinHandle;

use super::{EventSink, PendingEvent};

/// Specification for an artifact to persist alongside an event.
///
/// Mirrors the relevant columns of the `artifacts` table without coupling the
/// sink to the store's row types.
pub struct ArtifactSpec {
    pub kind: String,
    pub path: String,
    pub mime: String,
    pub metadata: serde_json::Value,
}

/// A unit of work for the writer thread.
enum StoreOp {
    /// Append a single event to the store.
    Event(PendingEvent),
    /// Append an event, then record an artifact tied to its sequence number.
    EventThenArtifact { ev: PendingEvent, art: ArtifactSpec },
}

/// Write-only, asynchronous [`EventSink`] backed by [`Store`].
pub struct StoreSink {
    tx: std_mpsc::Sender<StoreOp>,
}

impl StoreSink {
    /// Spawn the writer thread, returning the sink handle and a Tokio
    /// [`JoinHandle`] that completes when the writer thread has drained and
    /// exited. The writer runs until every `StoreSink` is dropped (closing the
    /// channel), then drains any remaining ops and exits.
    ///
    /// Takes `Arc<Store>` (frozen signature). Since `Arc<Store>` is not `Send`
    /// (the rusqlite `Connection` inside `Store` is not `Sync`), the `Arc` is
    /// unwrapped to an owned `Store` that is then moved onto the writer thread.
    /// The caller must hold the only reference at spawn time; if not, spawn
    /// falls back to a no-op writer rather than panicking.
    pub fn spawn(store: Arc<Store>) -> (Self, JoinHandle<()>) {
        let (tx, rx) = std_mpsc::channel::<StoreOp>();

        // The writer thread needs sole ownership of the (Send, !Sync) Store.
        let store = match Arc::try_unwrap(store) {
            Ok(store) => store,
            Err(_shared) => {
                // Another reference is live, so we cannot move the Store to a
                // worker thread. Return a sink whose ops are dropped and a task
                // that completes immediately.
                drop(rx);
                let handle = tokio::spawn(async {});
                return (Self { tx }, handle);
            }
        };

        // Dedicated OS thread owns the Store and applies ops sequentially.
        let writer = std::thread::spawn(move || {
            while let Ok(op) = rx.recv() {
                apply_op(&store, op);
            }
        });

        // Bridge the std-thread join into a Tokio JoinHandle without blocking a
        // runtime worker: the blocking `join()` runs on the blocking pool.
        let handle = tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = writer.join();
            })
            .await;
        });

        (Self { tx }, handle)
    }

    /// `append_event` THEN `record_artifact(seq)`.
    ///
    /// Enqueues an event-with-artifact op: the writer appends the event, then
    /// records the artifact against the event's returned sequence number.
    /// Fire-and-forget — a closed channel (writer already shut down) drops the
    /// op silently rather than panicking.
    pub fn emit_and_record(&self, ev: PendingEvent, art: ArtifactSpec) {
        let _ = self.tx.send(StoreOp::EventThenArtifact { ev, art });
    }
}

impl EventSink for StoreSink {
    fn emit(&self, ev: PendingEvent) {
        // Fire-and-forget: never block the turn loop, never panic on a closed
        // channel (writer thread already gone).
        let _ = self.tx.send(StoreOp::Event(ev));
    }
}

/// Apply a single [`StoreOp`] against the (synchronous) [`Store`].
///
/// Mirrors the legacy persistence ordering: append the event first, then record
/// any artifact anchored to the event's returned sequence number. Store errors
/// are best-effort and swallowed (see module docs); if the event append fails,
/// the artifact is skipped since it has no seq to anchor to.
fn apply_op(store: &Store, op: StoreOp) {
    match op {
        StoreOp::Event(ev) => {
            let _ = store.append_event(&ev.session_id, &ev.event_type, ev.payload);
        }
        StoreOp::EventThenArtifact { ev, art } => {
            if let Ok(record) = store.append_event(&ev.session_id, &ev.event_type, ev.payload) {
                let _ = store.record_artifact(
                    &ev.session_id,
                    Some(record.seq),
                    &art.kind,
                    &art.path,
                    Some(&art.mime),
                    art.metadata,
                );
            }
        }
    }
}
