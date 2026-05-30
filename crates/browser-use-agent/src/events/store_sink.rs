//! `store_sink.rs` — async writer task; the ONLY `EventSink` that touches the DB.

use super::{EventSink, PendingEvent};

pub struct ArtifactSpec {
    pub kind: String,
    pub path: String,
    pub mime: String,
    pub metadata: serde_json::Value,
}

pub struct StoreSink {
    // tx: UnboundedSender<StoreOp>
}

impl StoreSink {
    pub fn spawn(
        _store: std::sync::Arc<browser_use_store::Store>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        unimplemented!()
    }

    /// `append_event` THEN `record_artifact(seq)`.
    pub fn emit_and_record(&self, _ev: PendingEvent, _art: ArtifactSpec) {
        unimplemented!()
    }
}

impl EventSink for StoreSink {
    fn emit(&self, _ev: PendingEvent) {
        unimplemented!()
    }
}
