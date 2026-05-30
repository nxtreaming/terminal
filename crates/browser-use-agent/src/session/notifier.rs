//! `session/notifier.rs` — event-notify wakeups (NOT a 50ms poll).
//!
//! Frozen surface: `EventNotifier::subscribe` + the free `wait_for_events_after_seq` that
//! waits on a notification, then reads via `EventSource`.
//!
//! DECISION (WP-B4): `wait_for_events_after_seq` is genuinely event-driven. The store's
//! own `Store::wait_for_events_after_seq` is a 50ms `thread::sleep` poll loop, which we do
//! NOT use. Instead the store pushes `StoreNotification`s onto an `mpsc::Sender`
//! (`StoreNotifier`); `spawn_notifier_bridge` relays that mpsc into a
//! `tokio::sync::broadcast` so any number of async waiters can `subscribe()`. A waiter
//! parks on `broadcast::Receiver::recv()` (no polling, no sleeping) until the store
//! signals `EventsChanged` for its session, then reads the new rows via `EventSource`.

use crate::session::sink::{EventSeq, EventSource, SessionId};
use browser_use_protocol::EventRecord;
use browser_use_store::{StoreNotification, StoreNotifier};
use futures_util::future::BoxFuture;
use tokio::sync::broadcast;

/// Default capacity for the broadcast channel that fans store notifications out to waiters.
/// Lagged receivers are tolerated (see `wait_for_events_after_seq`).
pub const NOTIFIER_BROADCAST_CAPACITY: usize = 1024;

pub trait EventNotifier: Send + Sync {
    fn subscribe(&self) -> broadcast::Receiver<StoreNotification>;
}

/// Event-notify implementation over a `tokio::sync::broadcast`. Construct it with
/// [`spawn_notifier_bridge`], install the returned [`StoreNotifier`] on the `Store`
/// (`Store::set_notifier`) before sharing the store, then hand the `StoreEventNotifier`
/// to any waiters.
#[derive(Clone)]
pub struct StoreEventNotifier {
    sender: broadcast::Sender<StoreNotification>,
}

impl StoreEventNotifier {
    pub fn sender(&self) -> broadcast::Sender<StoreNotification> {
        self.sender.clone()
    }
}

impl EventNotifier for StoreEventNotifier {
    fn subscribe(&self) -> broadcast::Receiver<StoreNotification> {
        self.sender.subscribe()
    }
}

/// Bridge the store's synchronous `mpsc` notification channel onto an async-friendly
/// `tokio::sync::broadcast`. Returns:
/// - the [`StoreNotifier`] (`mpsc::Sender`) to install via `Store::set_notifier`, and
/// - a [`StoreEventNotifier`] that async waiters subscribe to.
///
/// A small relay task drains the blocking `mpsc::Receiver` and re-publishes each
/// notification onto the broadcast. The mpsc receive is blocking, so the relay runs on the
/// blocking pool. The task ends when the store (and thus the mpsc sender) is dropped.
pub fn spawn_notifier_bridge() -> (StoreNotifier, StoreEventNotifier) {
    let (mpsc_tx, mpsc_rx) = std::sync::mpsc::channel::<StoreNotification>();
    let (bcast_tx, _) = broadcast::channel::<StoreNotification>(NOTIFIER_BROADCAST_CAPACITY);
    let relay_tx = bcast_tx.clone();
    tokio::task::spawn_blocking(move || {
        // Blocks on the mpsc until the store-side sender is dropped, then exits.
        while let Ok(notification) = mpsc_rx.recv() {
            // A send error only means there are currently no subscribers; that is fine,
            // late subscribers re-check the log via `events_after_seq` on subscribe.
            let _ = relay_tx.send(notification);
        }
    });
    (mpsc_tx, StoreEventNotifier { sender: bcast_tx })
}

/// Wait until the store has events with `seq > after_seq` for `session_id`, then return them.
///
/// Event-notify, NOT a poll: we `subscribe()` first (so we cannot miss a notification that
/// races our initial read), do one immediate read to catch events already present, and
/// otherwise park on the broadcast until a relevant `EventsChanged`/`SessionChanged`
/// notification arrives before re-reading. On broadcast lag we simply re-read (the log is
/// the source of truth), so no notification can strand a waiter.
pub fn wait_for_events_after_seq<'a>(
    notifier: &'a dyn EventNotifier,
    source: &'a dyn EventSource,
    session_id: &'a SessionId,
    after_seq: EventSeq,
) -> BoxFuture<'a, anyhow::Result<Vec<EventRecord>>> {
    Box::pin(async move {
        // Subscribe BEFORE the first read to avoid a lost-wakeup race: any append that
        // lands after this point will deliver a notification we will observe.
        let mut rx = notifier.subscribe();

        loop {
            let events = source.events_after_seq(session_id, after_seq).await?;
            if !events.is_empty() {
                return Ok(events);
            }

            match rx.recv().await {
                Ok(notification) => {
                    if notification_targets_session(&notification, session_id) {
                        continue;
                    }
                    // Unrelated notification (other session / settings): keep waiting
                    // without re-reading the log.
                }
                // Lagged: we may have dropped notifications, so re-read to be safe.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                // All senders dropped (store gone): nothing more can ever arrive.
                Err(broadcast::error::RecvError::Closed) => {
                    return Ok(Vec::new());
                }
            }
        }
    })
}

fn notification_targets_session(notification: &StoreNotification, session_id: &SessionId) -> bool {
    match notification {
        StoreNotification::EventsChanged {
            session_id: sid, ..
        } => sid == session_id.as_str(),
        StoreNotification::SessionChanged { session_id: sid } => sid == session_id.as_str(),
        // Coarse notifications: re-check the log to be safe.
        StoreNotification::SessionsChanged => true,
        StoreNotification::SettingsChanged => false,
    }
}
