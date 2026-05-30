//! Event notification seam used to wake resume/coordination waiters without polling.
//!
//! Reconciliation (WP-A0): `browser_use_store::StoreNotification` exists, so the sketch's
//! `subscribe` payload type is kept. (The store's live notifier is an `mpsc::Sender`; a
//! broadcast adapter — or a tick channel — is wired up in WP-B4 against the concrete
//! `Store`. The trait here only fixes the contract.)

use super::sink::{EventSeq, EventSource, SessionId};
use browser_use_protocol::EventRecord;

pub trait EventNotifier: Send + Sync {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<browser_use_store::StoreNotification>;
}

pub async fn wait_for_events_after_seq(
    _n: &dyn EventNotifier,
    _src: &dyn EventSource,
    _s: &SessionId,
    _after: EventSeq,
) -> anyhow::Result<Vec<EventRecord>> {
    unimplemented!()
}
