//! Tests for the async task driver (`driver.rs`).
//!
//! These tests inject a fake [`SessionTask`] so the driver's spawn / replace /
//! abort lifecycle can be exercised deterministically without a real `TurnLoop`,
//! `ModelClient`, or network. A [`RecordingObserver`] captures the emitted
//! [`TurnLifecycleEvent`]s; timing-sensitive paths use real but sub-second
//! `tokio::time` sleeps so the graceful-then-hard 100ms window is exercised end
//! to end.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{InterruptMarkerConfig, LifecycleObserver, SessionTask, TaskDriver};
use crate::task::lifecycle::{
    InterruptedTurnHistoryMarker, TaskKind, TurnAbortReason, TurnLifecycleEvent,
};

/// Records lifecycle events for assertions.
#[derive(Clone)]
struct RecordingObserver {
    events: Arc<Mutex<Vec<TurnLifecycleEvent>>>,
}

impl RecordingObserver {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn events(&self) -> Vec<TurnLifecycleEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl LifecycleObserver for RecordingObserver {
    fn on_lifecycle(&self, ev: TurnLifecycleEvent) {
        self.events.lock().unwrap().push(ev);
    }
}

/// A controllable [`SessionTask`].
///
/// Behaviors:
/// - `Complete(value, delay)` — sleeps `delay`, then returns `value`.
/// - `Hang` — waits forever *unless* cancelled (ignores the token to model a
///   task that does NOT honor cancellation, forcing the hard-abort path).
/// - `Cooperative(delay_after_cancel)` — waits for cancellation, then sleeps a
///   short `delay_after_cancel` before returning (models a well-behaved task
///   that settles inside the graceful window).
enum Behavior {
    Complete {
        value: Option<String>,
        delay: Duration,
    },
    Hang,
    Cooperative {
        settle_after_cancel: Duration,
    },
}

struct ScriptedTask {
    behavior: Behavior,
    /// Set true when `run` actually started.
    ran: Arc<AtomicBool>,
    /// Incremented each time `abort()` cleanup runs.
    abort_calls: Arc<AtomicUsize>,
}

impl ScriptedTask {
    /// Returns the task *by value* (the driver wraps it in its own `Arc<T>`) plus
    /// a [`Probe`] sharing the same atomics, so the test can observe the task
    /// after it has been moved into the driver.
    fn new(behavior: Behavior) -> (Self, Probe) {
        let ran = Arc::new(AtomicBool::new(false));
        let abort_calls = Arc::new(AtomicUsize::new(0));
        let task = Self {
            behavior,
            ran: ran.clone(),
            abort_calls: abort_calls.clone(),
        };
        (task, Probe { ran, abort_calls })
    }
}

/// Out-of-band handles to observe a [`ScriptedTask`] after it has been moved
/// into the driver.
struct Probe {
    ran: Arc<AtomicBool>,
    abort_calls: Arc<AtomicUsize>,
}

impl Probe {
    fn ran(&self) -> bool {
        self.ran.load(Ordering::SeqCst)
    }
    fn abort_calls(&self) -> usize {
        self.abort_calls.load(Ordering::SeqCst)
    }
}

impl SessionTask for ScriptedTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(self: Arc<Self>, cancel: CancellationToken) -> Option<String> {
        self.ran.store(true, Ordering::SeqCst);
        match &self.behavior {
            Behavior::Complete { value, delay } => {
                tokio::time::sleep(*delay).await;
                value.clone()
            }
            Behavior::Hang => {
                // Ignore `cancel` on purpose: this task never settles on its own,
                // forcing the driver's hard-abort path. `futures::future::pending`
                // would never return; we use it to model a true hang.
                let _ = &cancel;
                std::future::pending::<()>().await;
                None
            }
            Behavior::Cooperative {
                settle_after_cancel,
            } => {
                // Poll for cancellation the same way the production turn loop
                // does (`is_cancelled()` — a Send-friendly sync check), rather
                // than awaiting the token's owned cancellation future (which is
                // `!Send` in this tokio-util version).
                while !cancel.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                tokio::time::sleep(*settle_after_cancel).await;
                None
            }
        }
    }

    async fn abort(&self) {
        self.abort_calls.fetch_add(1, Ordering::SeqCst);
    }
}

/// Poll until `pred` holds or `budget` elapses, so tests don't race the spawned
/// task. Uses real (short) sleeps; returns whether the predicate held.
async fn wait_until(budget: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if pred() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

// ---------------------------------------------------------------------------
// (1) start_task runs a task to completion → TurnStarted then TurnComplete.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn start_task_runs_to_completion_and_emits_lifecycle() {
    let observer = RecordingObserver::new();
    let driver = TaskDriver::with_observer(Arc::new(observer.clone()));

    let (task, probe) = ScriptedTask::new(Behavior::Complete {
        value: Some("hello from the task".to_string()),
        delay: Duration::from_millis(5),
    });
    // `start_task` takes the task by value; the driver wraps it in its own
    // `Arc<T>`. The `probe` shares the task's atomics so we can observe it.
    driver.start_task(task).await;

    // Wait for the spawned task to finish: on natural completion it emits
    // TurnComplete and then self-removes from the active slot (codex
    // `on_task_finished`). Waiting on the slot clearing is the strongest
    // post-condition and avoids racing the self-removal that happens just after
    // TurnComplete is emitted.
    let done = wait_until(Duration::from_secs(2), || !driver.has_active_turn()).await;
    assert!(done, "task never finished / cleared the active slot");

    assert!(probe.ran(), "task body never ran");
    assert!(
        observer
            .events()
            .iter()
            .any(|e| matches!(e, TurnLifecycleEvent::TurnComplete { .. })),
        "a naturally-completing task must emit TurnComplete"
    );

    let events = observer.events();
    assert_eq!(
        events.len(),
        2,
        "expected exactly TurnStarted + TurnComplete"
    );
    assert!(
        matches!(events[0], TurnLifecycleEvent::TurnStarted { .. }),
        "first event must be TurnStarted, got {:?}",
        events[0]
    );
    match &events[1] {
        TurnLifecycleEvent::TurnComplete {
            last_agent_message, ..
        } => {
            assert_eq!(
                last_agent_message.as_deref(),
                Some("hello from the task"),
                "TurnComplete must carry the task's result"
            );
        }
        other => panic!("second event must be TurnComplete, got {other:?}"),
    }
    assert_eq!(
        probe.abort_calls(),
        0,
        "a naturally-completing task must not have its abort() cleanup invoked"
    );
}

// ---------------------------------------------------------------------------
// (2) spawn_task while a task is active aborts the old (Replaced) before the new.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn spawn_task_replaces_active_task() {
    let observer = RecordingObserver::new();
    let driver = TaskDriver::with_observer(Arc::new(observer.clone()));

    // First task hangs so it is guaranteed still active when we replace it.
    let (first, first_probe) = ScriptedTask::new(Behavior::Hang);
    driver.start_task(first).await;
    assert!(
        wait_until(Duration::from_secs(2), || first_probe.ran()).await,
        "first task never started"
    );
    assert!(driver.has_active_turn(), "first task should be active");

    // Replace it with a quick second task.
    let (second, second_probe) = ScriptedTask::new(Behavior::Complete {
        value: Some("second".to_string()),
        delay: Duration::from_millis(5),
    });
    driver.spawn_task(second).await;

    // The old task was hard-aborted (didn't honor cancel) AND its abort() cleanup
    // ran exactly once.
    assert_eq!(
        first_probe.abort_calls(),
        1,
        "the replaced task's abort() cleanup must have run exactly once"
    );

    // Exactly one task should ever be active; wait for the second to finish and
    // self-clear the slot (the natural-completion post-condition).
    let done = wait_until(Duration::from_secs(2), || {
        !driver.has_active_turn()
            && observer.events().iter().any(|e| {
                matches!(
                    e,
                    TurnLifecycleEvent::TurnComplete { last_agent_message, .. }
                        if last_agent_message.as_deref() == Some("second")
                )
            })
    })
    .await;
    assert!(done, "second task never completed / cleared the slot");
    assert!(second_probe.ran(), "second task body never ran");
    assert!(
        !driver.has_active_turn(),
        "no task should remain active after the second completes"
    );

    // Lifecycle order: TurnStarted(1), TurnAborted(Replaced) for the first,
    // TurnStarted(2), TurnComplete(2).
    let events = observer.events();
    assert!(
        matches!(events[0], TurnLifecycleEvent::TurnStarted { .. }),
        "events[0] should be TurnStarted, got {:?}",
        events[0]
    );
    assert!(
        matches!(
            events[1],
            TurnLifecycleEvent::TurnAborted {
                reason: TurnAbortReason::Replaced,
                ..
            }
        ),
        "events[1] should be TurnAborted{{Replaced}}, got {:?}",
        events[1]
    );
    assert!(
        matches!(events[2], TurnLifecycleEvent::TurnStarted { .. }),
        "events[2] should be TurnStarted, got {:?}",
        events[2]
    );
    assert!(
        matches!(events[3], TurnLifecycleEvent::TurnComplete { .. }),
        "events[3] should be TurnComplete, got {:?}",
        events[3]
    );
    // Replaced aborts do NOT record an interrupted-turn history marker.
    assert!(
        driver.recorded_markers().is_empty(),
        "a Replaced abort must not record an interrupted-turn history marker"
    );
}

// ---------------------------------------------------------------------------
// (3) abort of a well-behaved task settles within the graceful window →
//     TurnAborted{Interrupted}, no hard abort needed.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn abort_well_behaved_task_settles_gracefully() {
    let observer = RecordingObserver::new();
    let driver = TaskDriver::with_observer(Arc::new(observer.clone()));

    // Cooperative task: settles ~10ms after cancellation — well inside the 100ms
    // graceful window.
    let (task, probe) = ScriptedTask::new(Behavior::Cooperative {
        settle_after_cancel: Duration::from_millis(10),
    });
    driver.start_task(task).await;
    assert!(
        wait_until(Duration::from_secs(2), || probe.ran()).await,
        "task never started"
    );

    let started = std::time::Instant::now();
    driver.abort_all_tasks(TurnAbortReason::Interrupted).await;
    let elapsed = started.elapsed();

    // It settled gracefully, so the abort returned well before the full graceful
    // window would have elapsed if a hard abort had been needed.
    assert!(
        elapsed < Duration::from_millis(100),
        "graceful abort should settle before the 100ms window elapses, took {elapsed:?}"
    );

    assert_eq!(
        probe.abort_calls(),
        1,
        "abort() cleanup must run exactly once on interrupt"
    );
    assert!(
        !driver.has_active_turn(),
        "active slot must be cleared after abort"
    );

    let events = observer.events();
    assert!(
        matches!(
            events.last(),
            Some(TurnLifecycleEvent::TurnAborted {
                reason: TurnAbortReason::Interrupted,
                ..
            })
        ),
        "last event must be TurnAborted{{Interrupted}}, got {:?}",
        events.last()
    );
    // No TurnComplete: a cancelled turn's completion is owned by the abort path.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TurnLifecycleEvent::TurnComplete { .. })),
        "a gracefully-aborted turn must not emit TurnComplete"
    );

    // Interrupt with the default policy (enabled, single-agent) records a
    // ContextualUser marker.
    assert_eq!(
        driver.recorded_markers(),
        vec![InterruptedTurnHistoryMarker::ContextualUser]
    );
}

// ---------------------------------------------------------------------------
// (4) abort of a HANGING task: graceful window elapses (~100ms) then hard abort
//     fires; the driver still returns and cleans up (test does not hang).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn abort_hanging_task_hard_aborts_after_graceful_window() {
    let observer = RecordingObserver::new();
    let driver = TaskDriver::with_observer(Arc::new(observer.clone()));

    let (task, probe) = ScriptedTask::new(Behavior::Hang);
    driver.start_task(task).await;
    assert!(
        wait_until(Duration::from_secs(2), || probe.ran()).await,
        "hanging task never started"
    );

    let started = std::time::Instant::now();
    // Bound the whole abort so a regression that hangs fails loudly instead of
    // wedging the test runner.
    let aborted = tokio::time::timeout(
        Duration::from_secs(5),
        driver.abort_all_tasks(TurnAbortReason::Interrupted),
    )
    .await;
    assert!(aborted.is_ok(), "abort_all_tasks hung on a runaway task");
    let elapsed = started.elapsed();

    // The graceful window (~100ms) must have elapsed before the hard abort — the
    // hanging task never settles on its own.
    assert!(
        elapsed >= Duration::from_millis(100),
        "hard abort should only fire after the graceful window elapses, took {elapsed:?}"
    );

    assert_eq!(
        probe.abort_calls(),
        1,
        "abort() cleanup must still run after a hard abort"
    );
    assert!(
        !driver.has_active_turn(),
        "active slot must be cleared after a hard abort"
    );

    let events = observer.events();
    assert!(
        matches!(
            events.last(),
            Some(TurnLifecycleEvent::TurnAborted {
                reason: TurnAbortReason::Interrupted,
                ..
            })
        ),
        "last event must be TurnAborted{{Interrupted}}, got {:?}",
        events.last()
    );
}

// ---------------------------------------------------------------------------
// (5) InterruptedTurnHistoryMarker::from_config truth table.
// ---------------------------------------------------------------------------
#[test]
fn interrupted_marker_from_config_truth_table() {
    use InterruptedTurnHistoryMarker as M;
    // interrupts disabled → no marker, regardless of multi-agent.
    assert_eq!(M::from_config(false, false), M::Disabled);
    assert_eq!(M::from_config(false, true), M::Disabled);
    // enabled, single-agent → contextual user marker.
    assert_eq!(M::from_config(true, false), M::ContextualUser);
    // enabled, multi-agent v2 → developer marker.
    assert_eq!(M::from_config(true, true), M::Developer);
}

// Bonus: an interrupt with a Disabled policy records no marker but still emits
// TurnAborted{Interrupted}.
#[tokio::test]
async fn interrupt_with_disabled_policy_records_no_marker() {
    let observer = RecordingObserver::new();
    let driver = TaskDriver::with_observer_and_config(
        Arc::new(observer.clone()),
        InterruptMarkerConfig {
            interrupt_enabled: false,
            multi_agent_v2: false,
        },
    );

    let (task, probe) = ScriptedTask::new(Behavior::Cooperative {
        settle_after_cancel: Duration::from_millis(5),
    });
    driver.start_task(task).await;
    assert!(wait_until(Duration::from_secs(2), || probe.ran()).await);

    driver.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(
        driver.recorded_markers().is_empty(),
        "disabled interrupt policy must record no history marker"
    );
    assert!(observer.events().iter().any(|e| matches!(
        e,
        TurnLifecycleEvent::TurnAborted {
            reason: TurnAbortReason::Interrupted,
            ..
        }
    )));
}
