//! Graceful-then-hard abort sequencing (codex `tasks/mod.rs:846`).
//!
//! The graceful-interruption timeout lives in the module root as
//! [`super::GRACEFULL_INTERRUPTION_TIMEOUT_MS`]. This module owns the *sequencing*
//! that [`super::driver::TaskDriver::abort_all_tasks`] performs against a single
//! active turn:
//!
//! 1. **Cancel** the active turn's [`CancellationToken`] so the task future
//!    observes cancellation cooperatively (`tasks/mod.rs:877`).
//! 2. **Wait, briefly,** for the task to settle on its own — race its `done`
//!    [`Notify`] against a [`GRACEFULL_INTERRUPTION_TIMEOUT_MS`] sleep
//!    (`tasks/mod.rs:846`). A well-behaved task that honors cancellation finishes
//!    inside the window; a hanging task does not.
//! 3. **Hard abort** the join handle if (and only if) the graceful window
//!    elapsed without the task settling.
//!
//! Keeping the sequencing here (rather than inline in the driver) keeps the
//! driver's `abort_all_tasks` readable and lets the behavior be reasoned about /
//! unit-described independently. The driver remains responsible for the *policy*
//! around it (already-cancelled short-circuit, calling `SessionTask::abort` for
//! cleanup, recording history markers, and emitting `TurnAborted`).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::GRACEFULL_INTERRUPTION_TIMEOUT_MS;

/// Cancel the active turn, wait up to the graceful window for it to settle, then
/// hard-abort the join handle if it overran (`tasks/mod.rs:846`).
///
/// Returns `true` if the task settled gracefully within the window, `false` if
/// the hard abort was required (the caller surfaces the hard kill via the
/// `TurnAborted` lifecycle event, so this returns the outcome rather than
/// logging it — the crate carries no logging facade). The caller is expected to
/// have already decided the token is not yet cancelled (the driver short-circuits
/// an already-cancelled active turn before calling this).
pub(super) async fn graceful_then_hard_abort(
    token: &CancellationToken,
    done: &Arc<Notify>,
    handle: &JoinHandle<()>,
) -> bool {
    // 1. Ask the task to stop (`tasks/mod.rs:877`).
    token.cancel();

    // 2. Race the task's `done` signal against the graceful window.
    //
    // `Notify::notified()` only observes *future* permits, so the driver fires
    // `done` exactly once when the spawned future returns. If the task already
    // finished and signalled before we got here, `notified()` would miss it — so
    // we re-check `is_finished()` first to avoid waiting on an already-completed
    // task.
    if handle.is_finished() {
        return true;
    }

    let settled = tokio::select! {
        biased;
        _ = done.notified() => true,
        _ = tokio::time::sleep(Duration::from_millis(GRACEFULL_INTERRUPTION_TIMEOUT_MS)) => false,
    };

    if settled {
        return true;
    }

    // 3. Graceful window elapsed: hard-abort the runaway task (`tasks/mod.rs`).
    handle.abort();
    false
}
