//! `task/` — async task driver / lifecycle.

pub mod abort;
pub mod driver;
pub mod lifecycle;

/// `tasks/mod.rs:64`.
pub const GRACEFULL_INTERRUPTION_TIMEOUT_MS: u64 = 100;

pub use driver::{SessionTask, TaskDriver};
pub use lifecycle::{InterruptedTurnHistoryMarker, TaskKind, TurnAbortReason, TurnLifecycleEvent};
