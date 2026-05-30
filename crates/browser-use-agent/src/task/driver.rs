//! Task driver: one active turn, spawn/replace/abort (codex `tasks/mod.rs` parity).
//!
//! The [`TaskDriver`] owns **at most one** active turn at a time. It is the async
//! lifecycle owner that codex's `tasks/mod.rs` `SessionTaskContext`/`ActiveTurn`
//! pair implements:
//!
//! - [`TaskDriver::spawn_task`] aborts any active turn with
//!   [`TurnAbortReason::Replaced`] and then starts the new one
//!   (`tasks/mod.rs:301-309`).
//! - [`TaskDriver::start_task`] snapshots, emits [`TurnLifecycleEvent::TurnStarted`]
//!   (`tasks/mod.rs:369`), creates a root [`CancellationToken`], `tokio::spawn`s
//!   the task's `run(arc_self, child_token)`, and on *normal* completion (token
//!   not cancelled) emits [`TurnLifecycleEvent::TurnComplete`]
//!   (`tasks/mod.rs:432/793`).
//! - [`TaskDriver::abort_all_tasks`] performs the graceful-then-hard interruption
//!   (`tasks/mod.rs:846-910`): cancel, wait up to
//!   [`GRACEFULL_INTERRUPTION_TIMEOUT_MS`], hard-abort if overrun, then call
//!   [`SessionTask::abort`] for cleanup; if the reason is
//!   [`TurnAbortReason::Interrupted`] it also records the interrupted-turn history
//!   marker and emits [`TurnLifecycleEvent::TurnAborted`].
//!
//! ## Type erasure
//!
//! The frozen [`SessionTask`] trait uses `self: Arc<Self>` + `-> impl Future` and
//! is therefore **not** object-safe. `spawn_task`/`start_task` stay generic over
//! `T: SessionTask`, but the single active-turn slot ([`ActiveTurn`]) must be
//! uniform. We erase `T` by capturing the `Arc<T>` into a boxed cleanup future
//! factory ([`AbortCleanup`]) when the turn is started; abort can then run the
//! task's own `abort()` without naming `T`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::abort::graceful_then_hard_abort;
use super::lifecycle::{
    InterruptedTurnHistoryMarker, TaskKind, TurnAbortReason, TurnLifecycleEvent,
};

/// One unit of cancellable, spawnable work owned by the [`TaskDriver`].
///
/// Frozen interface (`tasks/mod.rs` parity). The trait is *not* object-safe
/// (`self: Arc<Self>` + `-> impl Future`); the driver erases the concrete type
/// when a task becomes the active turn.
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

/// Sink for [`TurnLifecycleEvent`]s emitted by the driver.
///
/// This mirrors the `turn::TurnObserver` seam used by `TurnLoop`, but the task
/// driver needs an **object-safe** observer so the single active-turn slot can
/// hold one without leaking a type parameter onto [`TaskDriver`]. The blanket
/// impl below adapts any `turn::TurnObserver` so callers can reuse the same
/// concrete observer for both the loop and the driver.
pub trait LifecycleObserver: Send + Sync + 'static {
    fn on_lifecycle(&self, ev: TurnLifecycleEvent);
}

impl<O: crate::turn::TurnObserver> LifecycleObserver for O {
    fn on_lifecycle(&self, ev: TurnLifecycleEvent) {
        crate::turn::TurnObserver::on_lifecycle(self, ev)
    }
}

/// Policy controlling which interrupted-turn history marker the driver records on
/// an [`TurnAbortReason::Interrupted`] abort (`tasks/mod.rs:74`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptMarkerConfig {
    pub interrupt_enabled: bool,
    pub multi_agent_v2: bool,
}

impl InterruptMarkerConfig {
    fn marker(&self) -> InterruptedTurnHistoryMarker {
        InterruptedTurnHistoryMarker::from_config(self.interrupt_enabled, self.multi_agent_v2)
    }
}

impl Default for InterruptMarkerConfig {
    fn default() -> Self {
        // Single-agent interrupts enabled by default (records a ContextualUser
        // marker on interrupt), matching the common codex configuration.
        Self {
            interrupt_enabled: true,
            multi_agent_v2: false,
        }
    }
}

/// A boxed, type-erased cleanup future factory: runs the active task's own
/// `SessionTask::abort()` without the driver naming the concrete `T`.
type AbortCleanup = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// The single active turn the driver owns at any time (codex `ActiveTurn`).
struct ActiveTurn {
    turn_id: String,
    token: CancellationToken,
    done: Arc<Notify>,
    handle: JoinHandle<()>,
    /// Runs `SessionTask::abort()` for the active task. `Option` so it can be
    /// taken-and-run exactly once during teardown.
    cleanup: Option<AbortCleanup>,
}

/// Owns at most one active turn; spawns, replaces, and aborts it
/// (codex `tasks/mod.rs`).
pub struct TaskDriver {
    /// The single active-turn slot. Shared via `Arc` so the spawned task can
    /// self-remove on *natural* completion (codex `on_task_finished`), keeping
    /// at most one active task and making [`TaskDriver::has_active_turn`]
    /// truthful once a turn ends on its own.
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
    observer: Arc<dyn LifecycleObserver>,
    interrupt_marker: InterruptMarkerConfig,
    /// Records markers chosen on interrupt aborts. The real driver writes these
    /// into conversation history; here it is the seam that lets the policy be
    /// observed/asserted (the history write lands with the session integration).
    recorded_markers: Mutex<Vec<InterruptedTurnHistoryMarker>>,
}

impl TaskDriver {
    /// Construct a driver that swallows lifecycle events (no observer).
    ///
    /// Kept frozen-compatible (`new()` takes no args); use
    /// [`TaskDriver::with_observer`] to wire a real lifecycle sink.
    pub fn new() -> Self {
        Self::with_observer(Arc::new(NoopObserver))
    }

    /// Construct a driver that forwards lifecycle events to `observer`.
    pub fn with_observer(observer: Arc<dyn LifecycleObserver>) -> Self {
        Self::with_observer_and_config(observer, InterruptMarkerConfig::default())
    }

    /// Construct a driver with an explicit observer and interrupt-marker policy.
    pub fn with_observer_and_config(
        observer: Arc<dyn LifecycleObserver>,
        interrupt_marker: InterruptMarkerConfig,
    ) -> Self {
        Self {
            active_turn: Arc::new(Mutex::new(None)),
            observer,
            interrupt_marker,
            recorded_markers: Mutex::new(Vec::new()),
        }
    }

    /// `true` iff a turn is currently active.
    pub fn has_active_turn(&self) -> bool {
        self.active_turn.lock().unwrap().is_some()
    }

    /// Interrupted-turn history markers recorded so far (oldest first).
    pub fn recorded_markers(&self) -> Vec<InterruptedTurnHistoryMarker> {
        self.recorded_markers.lock().unwrap().clone()
    }

    /// Replace any active turn then start `task` (`tasks/mod.rs:301-309`).
    ///
    /// Codex guarantees at most one active task: it aborts the current one with
    /// [`TurnAbortReason::Replaced`] *before* installing the new one, and
    /// debug-asserts the slot is empty before adding.
    pub async fn spawn_task<T: SessionTask>(&self, task: T) {
        self.abort_all_tasks(TurnAbortReason::Replaced).await;
        debug_assert!(
            !self.has_active_turn(),
            "active turn slot must be empty after abort_all_tasks before starting a new task"
        );
        self.start_task(task).await;
    }

    /// Start `task` as the active turn (`tasks/mod.rs:369`).
    ///
    /// Snapshots a turn id, emits [`TurnLifecycleEvent::TurnStarted`], builds a
    /// root [`CancellationToken`], `tokio::spawn`s `run(arc_self, child_token)`,
    /// and installs the active-turn slot. On *normal* completion (token not
    /// cancelled) the spawned future emits [`TurnLifecycleEvent::TurnComplete`].
    pub async fn start_task<T: SessionTask>(&self, task: T) {
        let turn_id = new_turn_id();

        // 1. Lifecycle: a turn is starting (`tasks/mod.rs:369`).
        self.observer.on_lifecycle(TurnLifecycleEvent::TurnStarted {
            turn_id: turn_id.clone(),
        });

        // 2. Cancellation: root token; the task runs on a child so the driver
        //    can cancel the whole subtree (`tasks/mod.rs`).
        let token = CancellationToken::new();
        let child = token.child_token();

        // 3. `done` notify: fired exactly once when the spawned future returns,
        //    so the abort path can race settlement against the graceful window.
        let done = Arc::new(Notify::new());

        let arc_task: Arc<T> = Arc::new(task);

        // Cleanup factory: runs the task's own `abort()` during teardown.
        let cleanup_task = arc_task.clone();
        let cleanup: AbortCleanup =
            Box::new(move || Box::pin(async move { cleanup_task.abort().await }));

        // 4. Spawn the task. On normal completion (NOT cancelled) emit
        //    `TurnComplete`, self-remove from the active slot (codex
        //    `on_task_finished`), then fire `done` last so abort can observe it.
        let observer = self.observer.clone();
        let spawn_token = child.clone();
        let spawn_done = done.clone();
        let spawn_turn_id = turn_id.clone();
        let run_task = arc_task.clone();
        let slot_for_task = self.active_turn.clone();
        let handle = tokio::spawn(async move {
            let last_agent_message = run_task.run(spawn_token.clone()).await;

            // Only a turn that ran to completion *without* being cancelled emits
            // `TurnComplete` (`tasks/mod.rs:432/793`) and self-removes. A
            // cancelled turn's teardown is owned by the abort path (which already
            // `take()`-ed the slot and emits `TurnAborted`); it must NOT touch the
            // slot here, or it could clobber a replacement turn.
            if !spawn_token.is_cancelled() {
                observer.on_lifecycle(TurnLifecycleEvent::TurnComplete {
                    turn_id: spawn_turn_id.clone(),
                    last_agent_message,
                });

                // Self-remove, but only if the slot still holds *this* turn — a
                // concurrent `spawn_task`/abort may already have replaced/taken it.
                let mut slot = slot_for_task.lock().unwrap();
                if slot.as_ref().is_some_and(|a| a.turn_id == spawn_turn_id) {
                    *slot = None;
                }
            }

            // Signal settlement last so a racing abort sees `done` only after the
            // task body (and any `TurnComplete` / self-removal) has fully run.
            spawn_done.notify_one();
        });

        // 5. Install the active-turn slot.
        let mut slot = self.active_turn.lock().unwrap();
        debug_assert!(
            slot.is_none(),
            "start_task must not overwrite an existing active turn"
        );
        *slot = Some(ActiveTurn {
            turn_id,
            token,
            done,
            handle,
            cleanup: Some(cleanup),
        });
    }

    /// Tear down the active turn, if any (`tasks/mod.rs:846-910`).
    ///
    /// Sequence (codex parity):
    /// 1. If there is no active turn, or its token is already cancelled, return.
    /// 2. Cancel + wait up to [`GRACEFULL_INTERRUPTION_TIMEOUT_MS`] for the task
    ///    to settle; hard-abort the join handle on overrun (see
    ///    [`graceful_then_hard_abort`]).
    /// 3. Run the task's own [`SessionTask::abort`] cleanup.
    /// 4. If `reason == Interrupted`, record the interrupted-turn history marker
    ///    and emit [`TurnLifecycleEvent::TurnAborted`].
    pub async fn abort_all_tasks(&self, reason: TurnAbortReason) {
        // Take the active turn out of the slot. Holding the lock only to swap
        // keeps the await points lock-free.
        let active = {
            let mut slot = self.active_turn.lock().unwrap();
            // Already-cancelled short-circuit (`tasks/mod.rs`): if a teardown is
            // already in flight (token cancelled) leave it to that caller.
            match slot.as_ref() {
                None => return,
                Some(active) if active.token.is_cancelled() => return,
                Some(_) => {}
            }
            slot.take()
        };

        let Some(mut active) = active else {
            return;
        };

        // 2. Graceful-then-hard interruption (`tasks/mod.rs:846`).
        graceful_then_hard_abort(&active.token, &active.done, &active.handle).await;

        // 3. Task-specific cleanup (`SessionTask::abort`).
        if let Some(cleanup) = active.cleanup.take() {
            cleanup().await;
        }

        // 4. Interrupt-only: record history marker + emit `TurnAborted`
        //    (`tasks/mod.rs:877-904`). Replaced/Error tear down silently here;
        //    the `Replaced` path is driven by `spawn_task` installing a new turn,
        //    which already emitted its own `TurnStarted`.
        //
        // The marker is recorded only when the configured policy is *not*
        // `Disabled` (codex skips the history write when interrupts are disabled).
        if reason == TurnAbortReason::Interrupted {
            let marker = self.interrupt_marker.marker();
            if marker != InterruptedTurnHistoryMarker::Disabled {
                self.recorded_markers.lock().unwrap().push(marker);
            }
        }

        self.observer.on_lifecycle(TurnLifecycleEvent::TurnAborted {
            turn_id: active.turn_id,
            reason,
        });
    }
}

impl Default for TaskDriver {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`LifecycleObserver`] that drops every event (default driver observer).
struct NoopObserver;

impl LifecycleObserver for NoopObserver {
    fn on_lifecycle(&self, _ev: TurnLifecycleEvent) {}
}

/// Mint a fresh, unique turn id.
///
/// Codex keys turns by a monotonically increasing id; here a process-local
/// counter is sufficient (and deterministic enough for tests that only assert
/// *which* lifecycle events fired, not their exact ids).
fn new_turn_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("turn-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod driver_tests;
