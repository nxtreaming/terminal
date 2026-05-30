//! `task/` — async task driver / lifecycle.
//!
//! This module is the async lifecycle owner for a session's *active turn*. It
//! mirrors codex's `tasks/mod.rs`: at most one task is active at a time, starting
//! a new task replaces (aborts) the current one, and tearing a turn down follows
//! a graceful-then-hard interruption protocol.
//!
//! - [`lifecycle`] — pure value types ([`TurnAbortReason`], [`TaskKind`],
//!   [`TurnLifecycleEvent`], [`InterruptedTurnHistoryMarker`]).
//! - [`abort`] — the graceful-then-hard abort *sequencing*.
//! - [`driver`] — the [`TaskDriver`] (spawn / start / abort) and the
//!   [`SessionTask`] trait.

pub mod abort;
pub mod driver;
pub mod lifecycle;

/// `tasks/mod.rs:64`.
pub const GRACEFULL_INTERRUPTION_TIMEOUT_MS: u64 = 100;

pub use driver::{InterruptMarkerConfig, LifecycleObserver, SessionTask, TaskDriver};
pub use lifecycle::{InterruptedTurnHistoryMarker, TaskKind, TurnAbortReason, TurnLifecycleEvent};
