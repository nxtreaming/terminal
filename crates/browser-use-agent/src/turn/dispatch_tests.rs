//! Tests for the async in-turn [`ToolDispatcher`] (WP-C1).
//!
//! NETWORK-FREE: every test injects a [`ScriptedRunner`] in place of the real
//! [`OrchestratorRunner`]/[`tools::ToolOrchestrator`]. No `ModelClient`, sandbox,
//! or socket is ever touched. The scripted runner:
//! - records the order in which calls are *invoked*,
//! - tracks a live concurrency counter (and its max) so a test can prove that
//!   parallel-safe calls actually overlap and serial calls don't,
//! - can sleep per-call (so a later call can finish before an earlier one,
//!   proving the output ordering is by *model order*, not completion order),
//! - returns a canned [`Message`] tagged with the call id.
//!
//! Sleeps are short (tens of ms) and deterministic; `tokio`'s `start_paused` is
//! intentionally not required, so no Cargo manifest change is needed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use tokio_util::sync::CancellationToken;

use crate::turn::dispatch::{CallRunner, ToolDispatcher};

// ---- scripted runner ------------------------------------------------------

/// Per-call script: how long the call sleeps and whether it is parallel-safe.
#[derive(Clone)]
struct CallScript {
    /// Tool call id (used to key the returned message + assert ordering).
    id: String,
    /// How long the call "runs" before returning.
    sleep: Duration,
    /// codex `ToolRuntime::parallel_safe` for this call.
    parallel_safe: bool,
}

/// A [`CallRunner`] that replays a per-call script and instruments concurrency.
struct ScriptedRunner {
    scripts: Mutex<std::collections::HashMap<String, CallScript>>,
    /// Order in which calls were *invoked* (push of the call id at run start).
    invocation_order: Mutex<Vec<String>>,
    /// Live count of in-flight `run` calls.
    in_flight: AtomicUsize,
    /// High-water mark of `in_flight` (proves overlap / exclusivity).
    max_concurrency: AtomicUsize,
    /// Number of `run` calls that actually started (proves cancel stops scheduling).
    runs_started: AtomicUsize,
}

impl ScriptedRunner {
    fn new(scripts: Vec<CallScript>) -> Arc<Self> {
        let map = scripts.into_iter().map(|s| (s.id.clone(), s)).collect();
        Arc::new(Self {
            scripts: Mutex::new(map),
            invocation_order: Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            max_concurrency: AtomicUsize::new(0),
            runs_started: AtomicUsize::new(0),
        })
    }

    fn max_concurrency(&self) -> usize {
        self.max_concurrency.load(Ordering::SeqCst)
    }
    fn runs_started(&self) -> usize {
        self.runs_started.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CallRunner for ScriptedRunner {
    fn parallel_safe(&self, call: &ContentPart) -> bool {
        let id = call_id(call);
        self.scripts
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.parallel_safe)
            .unwrap_or(false)
    }

    async fn run(&self, call: ContentPart) -> Message {
        let id = call_id(&call);
        self.runs_started.fetch_add(1, Ordering::SeqCst);
        self.invocation_order.lock().unwrap().push(id.clone());

        // Enter the critical "running" window and bump the high-water mark.
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrency.fetch_max(now, Ordering::SeqCst);

        let sleep = self
            .scripts
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.sleep)
            .unwrap_or(Duration::ZERO);
        if !sleep.is_zero() {
            tokio::time::sleep(sleep).await;
        }

        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        canned_output(&id)
    }
}

// ---- helpers --------------------------------------------------------------

/// Build a model tool-call content part with the given id (the dispatcher input).
fn tool_call(id: &str) -> ContentPart {
    ContentPart::ToolCall {
        id: id.to_string(),
        name: "shell".to_string(),
        input: serde_json::json!({ "command": ["echo", id] }),
        provider_metadata: None,
    }
}

/// Extract the call id from a `ToolCall` content part.
fn call_id(call: &ContentPart) -> String {
    match call {
        ContentPart::ToolCall { id, .. } => id.clone(),
        _ => String::new(),
    }
}

/// The canned output message a scripted call returns, tagged with its call id so
/// a test can assert the *output order* matches the *input order*.
fn canned_output(id: &str) -> Message {
    Message::new(
        MessageRole::Tool,
        vec![ContentPart::ToolResult {
            tool_call_id: id.to_string(),
            content: vec![ContentPart::text(format!("output:{id}"))],
            is_error: false,
        }],
    )
}

/// Read the `tool_call_id` out of a recorded output message (for ordering asserts).
fn output_id(msg: &Message) -> String {
    for part in &msg.content {
        if let ContentPart::ToolResult { tool_call_id, .. } = part {
            return tool_call_id.clone();
        }
    }
    String::new()
}

fn script(id: &str, sleep_ms: u64, parallel_safe: bool) -> CallScript {
    CallScript {
        id: id.to_string(),
        sleep: Duration::from_millis(sleep_ms),
        parallel_safe,
    }
}

// ---- (1) outputs recorded in MODEL order, not completion order ------------

#[tokio::test]
async fn outputs_are_recorded_in_model_order_even_when_later_calls_finish_first() {
    // call[0] sleeps far longer than call[1], so call[1] *completes first*. The
    // FuturesOrdered drain must still yield [a, b] in model order. Both are
    // parallel-safe so they actually run concurrently (otherwise call[1] couldn't
    // even start before call[0] finished).
    let runner = ScriptedRunner::new(vec![script("a", 80, true), script("b", 5, true)]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), /* model_supports */ true);

    let out = dispatcher
        .dispatch_ordered(
            vec![tool_call("a"), tool_call("b")],
            CancellationToken::new(),
        )
        .await;

    let order: Vec<String> = out.outputs_in_order.iter().map(output_id).collect();
    assert_eq!(
        order,
        vec!["a".to_string(), "b".to_string()],
        "outputs must be in MODEL order [a, b] even though b finishes before a"
    );
    assert!(out.needs_follow_up, "dispatched calls -> needs_follow_up");
}

// ---- (2) parallel-safe calls actually overlap -----------------------------

#[tokio::test]
async fn parallel_safe_calls_overlap() {
    // Three parallel-safe calls that all sleep: they should be in-flight at the
    // same time, so observed max concurrency must exceed 1.
    let runner = ScriptedRunner::new(vec![
        script("a", 60, true),
        script("b", 60, true),
        script("c", 60, true),
    ]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let out = dispatcher
        .dispatch_ordered(
            vec![tool_call("a"), tool_call("b"), tool_call("c")],
            CancellationToken::new(),
        )
        .await;

    assert_eq!(out.outputs_in_order.len(), 3);
    assert!(
        runner.max_concurrency() > 1,
        "parallel-safe calls must overlap (max_concurrency={}, expected >1)",
        runner.max_concurrency()
    );
}

// ---- (3) a serial call forces exclusivity ---------------------------------

#[tokio::test]
async fn serial_call_is_exclusive_and_does_not_overlap() {
    // Every call is parallel-*unsafe* (serial), so each takes a write guard on the
    // shared RwLock: they must run strictly one-at-a-time. Max concurrency == 1.
    let runner = ScriptedRunner::new(vec![
        script("a", 40, false),
        script("b", 40, false),
        script("c", 40, false),
    ]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let out = dispatcher
        .dispatch_ordered(
            vec![tool_call("a"), tool_call("b"), tool_call("c")],
            CancellationToken::new(),
        )
        .await;

    assert_eq!(out.outputs_in_order.len(), 3);
    assert_eq!(
        runner.max_concurrency(),
        1,
        "serial calls must NOT overlap (max_concurrency must be 1)"
    );
    // Even serial, outputs stay in model order.
    let order: Vec<String> = out.outputs_in_order.iter().map(output_id).collect();
    assert_eq!(order, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn model_without_parallel_support_forces_serial_even_for_parallel_safe_calls() {
    // Calls are individually parallel-safe, but the *model* does not support
    // parallel tool calls (`classify_parallelism(true, false) == Serial`), so they
    // must run exclusively. This proves the model-capability half of the gate.
    let runner = ScriptedRunner::new(vec![script("a", 40, true), script("b", 40, true)]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), /* model_supports */ false);

    let out = dispatcher
        .dispatch_ordered(
            vec![tool_call("a"), tool_call("b")],
            CancellationToken::new(),
        )
        .await;

    assert_eq!(out.outputs_in_order.len(), 2);
    assert_eq!(
        runner.max_concurrency(),
        1,
        "model without parallel support must serialize parallel-safe calls"
    );
}

// ---- (4) cancellation mid-dispatch stops further calls --------------------

#[tokio::test]
async fn cancellation_mid_dispatch_stops_scheduling_further_calls() {
    // Serial calls (exclusive), each sleeping. We cancel shortly after dispatch
    // starts: the first call is already in-flight, but scheduling of the remaining
    // calls must stop, so far fewer than all 5 calls ever start.
    let runner = ScriptedRunner::new(vec![
        script("a", 60, false),
        script("b", 60, false),
        script("c", 60, false),
        script("d", 60, false),
        script("e", 60, false),
    ]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        // Let the first serial call get in-flight, then cancel.
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel2.cancel();
    });

    let calls = vec![
        tool_call("a"),
        tool_call("b"),
        tool_call("c"),
        tool_call("d"),
        tool_call("e"),
    ];
    let _out = dispatcher.dispatch_ordered(calls, cancel).await;

    // Because cancellation stops scheduling new calls AND in-flight calls
    // short-circuit on cancel, strictly fewer than all 5 calls ever started.
    let started = runner.runs_started();
    assert!(
        started < 5,
        "cancellation must stop scheduling further calls (runs_started={started}, expected <5)"
    );
}

// ---- (5) needs_follow_up: true when dispatched, false for empty -----------

#[tokio::test]
async fn needs_follow_up_true_when_calls_dispatched_false_for_empty() {
    let runner = ScriptedRunner::new(vec![script("a", 1, true)]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let out = dispatcher
        .dispatch_ordered(vec![tool_call("a")], CancellationToken::new())
        .await;
    assert!(
        out.needs_follow_up,
        ">=1 dispatched call -> needs_follow_up=true"
    );
    assert_eq!(out.outputs_in_order.len(), 1);

    // Empty input: nothing dispatched, no follow-up.
    let empty_runner = ScriptedRunner::new(vec![]);
    let empty_dispatcher = ToolDispatcher::with_runner(empty_runner.clone(), true);
    let empty = empty_dispatcher
        .dispatch_ordered(vec![], CancellationToken::new())
        .await;
    assert!(
        !empty.needs_follow_up,
        "empty input -> needs_follow_up=false"
    );
    assert!(empty.outputs_in_order.is_empty());
    assert_eq!(empty_runner.runs_started(), 0);
}
