//! Task driver: one active turn, spawn/replace/abort (codex `tasks/mod.rs` parity).

use super::lifecycle::{TaskKind, TurnAbortReason};
use tokio_util::sync::CancellationToken;

pub trait SessionTask: Send + Sync + 'static {
    fn kind(&self) -> TaskKind;
    fn run(
        self: std::sync::Arc<Self>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Option<String>> + Send;
    fn abort(&self) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }
}

pub struct TaskDriver {
    // active_turn: Mutex<Option<ActiveTurn>>, observer
}

impl TaskDriver {
    pub fn new() -> Self {
        unimplemented!()
    }

    /// `abort_all(Replaced)` + start (`tasks/mod.rs:301`).
    pub async fn spawn_task<T: SessionTask>(&self, _task: T) {
        unimplemented!()
    }

    /// `TurnStarted`, `tokio::spawn`, done `Notify` (`tasks/mod.rs`).
    pub async fn start_task<T: SessionTask>(&self, _task: T) {
        unimplemented!()
    }

    /// Graceful 100ms then `handle.abort` (`tasks/mod.rs:846`).
    pub async fn abort_all_tasks(&self, _reason: TurnAbortReason) {
        unimplemented!()
    }
}

impl Default for TaskDriver {
    fn default() -> Self {
        Self::new()
    }
}
