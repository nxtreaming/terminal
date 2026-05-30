//! `session/sink.rs` — durable event sink/source over `browser_use_store::Store`.
//!
//! Frozen surface: `EventSink` (append-only write path) + `EventSource` (replay read path),
//! plus the `SessionId` / `EventSeq` newtypes.
//!
//! WP-B4 backs the frozen traits with `StoreEventSink` / `StoreEventSource`, both wrapping a
//! `SharedStore = Arc<Mutex<Store>>`. The store is synchronous (rusqlite), so every call is
//! dispatched through `tokio::task::spawn_blocking` to keep the async runtime unblocked.
//! `rusqlite::Connection` is `Send + !Sync`, so the store lives behind a `std::sync::Mutex`
//! to satisfy the `Send + Sync` bound on the traits and to serialize the single connection.
//!
//! `EventSink` is APPEND-ONLY (the durable write path); `EventSource` reads only, and only
//! during resume / fork / rollback replay. In-memory provider history (in `Session`) is the
//! authoritative live history — SQLite is the durable log we replay from.
//!
//! NOTE on session ids: the real `browser_use_store::Store` mints session ids itself in
//! `create_session` / `create_child_session` (returning `SessionMeta`), and the `events`
//! table has a foreign key on `sessions(id)`. So session-row creation lives on the SINK
//! (`create_session` / the frozen `create_child_session`), which returns the store-assigned
//! [`SessionId`]. `Session::create`/`fork` call those and adopt the returned id.

use browser_use_protocol::EventRecord;
use browser_use_store::Store;
use futures_util::future::BoxFuture;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub type EventSeq = i64;

/// Shared, thread-safe handle to the synchronous store. Cloned cheaply across the sink,
/// source and notifier so they all observe the same SQLite connection / notifications.
pub type SharedStore = Arc<Mutex<Store>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub trait EventSink: Send + Sync {
    /// Append an event to a session's durable log (append-only; never reads/updates events).
    fn append_event<'a>(
        &'a self,
        session_id: &'a SessionId,
        event_type: &'a str,
        payload: &'a serde_json::Value,
    ) -> BoxFuture<'a, anyhow::Result<EventRecord>>;

    /// Create a child session row under `parent` and return its store-assigned id.
    fn create_child_session<'a>(
        &'a self,
        parent: &'a SessionId,
        cwd: PathBuf,
        agent_path: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<SessionId>>;

    /// Create a brand-new ROOT session row and return its store-assigned id.
    ///
    /// Root-create is a one-time setup write, not part of the per-turn append path, but it
    /// lives on the trait (rather than only on the concrete sink) so `Session::create` can
    /// stay against `Arc<dyn EventSink>`. Non-store sinks may leave the default, which
    /// errors — only the durable `StoreEventSink` mints durable root rows.
    fn create_root_session(&self, _cwd: PathBuf) -> BoxFuture<'_, anyhow::Result<SessionId>> {
        Box::pin(async {
            Err(anyhow::anyhow!(
                "this EventSink does not support root-session creation; use a StoreEventSink"
            ))
        })
    }
}

pub trait EventSource: Send + Sync {
    fn events_for_session<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> BoxFuture<'a, anyhow::Result<Vec<EventRecord>>>;

    fn events_after_seq<'a>(
        &'a self,
        session_id: &'a SessionId,
        after_seq: EventSeq,
    ) -> BoxFuture<'a, anyhow::Result<Vec<EventRecord>>>;
}

/// Append-only durable write path, backed by `browser_use_store::Store`.
#[derive(Clone)]
pub struct StoreEventSink {
    store: SharedStore,
}

impl StoreEventSink {
    pub fn new(store: SharedStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> SharedStore {
        Arc::clone(&self.store)
    }

    /// Create a brand-new root session row and return its store-assigned id. Convenience
    /// inherent wrapper over the [`EventSink::create_root_session`] trait method.
    pub async fn create_session(&self, cwd: PathBuf) -> anyhow::Result<SessionId> {
        self.create_root_session(cwd).await
    }
}

impl EventSink for StoreEventSink {
    fn append_event<'a>(
        &'a self,
        session_id: &'a SessionId,
        event_type: &'a str,
        payload: &'a serde_json::Value,
    ) -> BoxFuture<'a, anyhow::Result<EventRecord>> {
        let store = Arc::clone(&self.store);
        let session_id = session_id.0.clone();
        let event_type = event_type.to_string();
        let payload = payload.clone();
        Box::pin(async move {
            spawn_blocking_store(move || {
                let store = store.lock().expect("store mutex poisoned");
                store.append_event(&session_id, &event_type, payload)
            })
            .await
        })
    }

    fn create_child_session<'a>(
        &'a self,
        parent: &'a SessionId,
        cwd: PathBuf,
        agent_path: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<SessionId>> {
        let store = Arc::clone(&self.store);
        let parent = parent.0.clone();
        let agent_path = agent_path.map(ToOwned::to_owned);
        Box::pin(async move {
            spawn_blocking_store(move || {
                let store = store.lock().expect("store mutex poisoned");
                let meta =
                    store.create_child_session(&parent, &cwd, agent_path.as_deref(), None, None)?;
                Ok(SessionId(meta.id))
            })
            .await
        })
    }

    fn create_root_session(&self, cwd: PathBuf) -> BoxFuture<'_, anyhow::Result<SessionId>> {
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            spawn_blocking_store(move || {
                let store = store.lock().expect("store mutex poisoned");
                let meta = store.create_session(None, &cwd)?;
                Ok(SessionId(meta.id))
            })
            .await
        })
    }
}

/// Read-only replay path, backed by `browser_use_store::Store`. Reads only during resume,
/// fork and rollback — never mutates the log.
#[derive(Clone)]
pub struct StoreEventSource {
    store: SharedStore,
}

impl StoreEventSource {
    pub fn new(store: SharedStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> SharedStore {
        Arc::clone(&self.store)
    }
}

impl EventSource for StoreEventSource {
    fn events_for_session<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> BoxFuture<'a, anyhow::Result<Vec<EventRecord>>> {
        let store = Arc::clone(&self.store);
        let session_id = session_id.0.clone();
        Box::pin(async move {
            spawn_blocking_store(move || {
                let store = store.lock().expect("store mutex poisoned");
                store.events_for_session(&session_id)
            })
            .await
        })
    }

    fn events_after_seq<'a>(
        &'a self,
        session_id: &'a SessionId,
        after_seq: EventSeq,
    ) -> BoxFuture<'a, anyhow::Result<Vec<EventRecord>>> {
        let store = Arc::clone(&self.store);
        let session_id = session_id.0.clone();
        Box::pin(async move {
            spawn_blocking_store(move || {
                let store = store.lock().expect("store mutex poisoned");
                store.events_after_seq(&session_id, after_seq)
            })
            .await
        })
    }
}

/// Run a synchronous store closure on the blocking pool and flatten the join error.
async fn spawn_blocking_store<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|join_err| anyhow::anyhow!("store task panicked: {join_err}"))?
}
