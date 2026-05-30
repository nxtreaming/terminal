//! Write-sink boundary: the store is a SINK at runtime and a SOURCE only at resume/fork.

use browser_use_protocol::EventRecord;
use std::path::Path;

pub struct SessionId(pub String);
pub type EventSeq = i64;

/// Runtime: append-only, NEVER read.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    async fn append_event(
        &self,
        session: &SessionId,
        ty: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<EventSeq>;
    async fn create_child_session(
        &self,
        parent: &SessionId,
        cwd: &Path,
        agent_path: Option<&str>,
    ) -> anyhow::Result<SessionId>;
}

/// Resume/fork-time ONLY.
#[async_trait::async_trait]
pub trait EventSource: Send + Sync {
    async fn events_for_session(&self, session: &SessionId) -> anyhow::Result<Vec<EventRecord>>;
    async fn events_after_seq(
        &self,
        session: &SessionId,
        after: EventSeq,
    ) -> anyhow::Result<Vec<EventRecord>>;
}
