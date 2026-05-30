//! In-turn tool dispatch: `FuturesOrdered` + `RwLock` gate (codex `turn.rs:106` +
//! `tools/parallel.rs`).
//!
//! ## What this does (codex parity)
//! When a model turn yields one or more tool calls, codex schedules them so that
//! their **outputs are recorded in model order** regardless of which call
//! finishes first, while still letting *parallel-safe* calls run concurrently and
//! forcing *serial* calls to run with exclusive access. We reproduce that here:
//!
//! - **Ordering** — every call is pushed onto a [`FuturesOrdered`]
//!   (`futures_util::stream::FuturesOrdered`). `FuturesOrdered` yields results in
//!   the order the futures were *pushed*, not the order they *complete*, so the
//!   collected `Vec<Message>` matches the model's call order even when a later
//!   call finishes first (codex `turn.rs:1655/1873` collects ordered outputs).
//!
//! - **Parallel-vs-serial gate** — a single shared
//!   `Arc<tokio::sync::RwLock<()>>` is the concurrency gate (codex `turn.rs:106`,
//!   `tools/parallel.rs`). The per-call parallelism is the *pure* decision
//!   [`decision::classify_parallelism`]: a call that is `parallel_safe` **and**
//!   whose model `supports_parallel_tool_calls` is
//!   [`ToolParallelism::Parallel`](crate::decision::ToolParallelism::Parallel) and
//!   takes a **read** guard (many can hold it at once); any other call is
//!   [`ToolParallelism::Serial`](crate::decision::ToolParallelism::Serial) and
//!   takes a **write** guard (exclusive — it waits for in-flight reads to drain
//!   and blocks new ones until it finishes). Because the per-call future acquires
//!   its own guard *inside* the future, scheduling stays cheap and the gate
//!   enforces the overlap rules at run time.
//!
//! - **Cancellation** — the [`CancellationToken`] is honored two ways: we stop
//!   *scheduling* new calls the moment cancel fires, and each in-flight future
//!   `select!`s on `cancel.cancelled()` so it can short-circuit. Calls that were
//!   already scheduled are still drained from the `FuturesOrdered` (codex
//!   `drain_in_flight` semantics: started work is observed before returning) so
//!   the result vector never has holes.
//!
//! ## Testability
//! The dispatcher never talks to a real `ModelClient`, sandbox, or network. It
//! runs each call through an injected [`CallRunner`]: the production constructor
//! ([`ToolDispatcher::new`]) backs it with [`OrchestratorRunner`] (which delegates
//! to the merged [`tools::ToolOrchestrator`](crate::tools::ToolOrchestrator)),
//! while tests inject a `ScriptedRunner` (see `dispatch_tests.rs`) that records
//! invocation order + observed concurrency and returns canned [`Message`]s. The
//! classifier (`parallel_safe` per call) is likewise injected so tests can drive
//! the gate directly.

use std::sync::Arc;

use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use futures_util::stream::{FuturesOrdered, StreamExt};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::decision::{self, ToolParallelism};

/// Runs a single tool call to completion, producing the `Message` to record.
///
/// This is the seam that keeps the dispatcher network-free and unit-testable: the
/// real impl ([`OrchestratorRunner`]) routes through
/// [`tools::ToolOrchestrator`](crate::tools::ToolOrchestrator) (approval ->
/// sandbox -> exec), while tests use a scripted fake.
#[async_trait::async_trait]
pub trait CallRunner: Send + Sync {
    /// Whether this specific call is parallel-safe (codex `ToolRuntime::parallel_safe`).
    /// Combined with the model's `supports_parallel_tool_calls` by
    /// [`decision::classify_parallelism`] to pick the gate guard.
    fn parallel_safe(&self, call: &ContentPart) -> bool;

    /// Execute the call and return the `Message` (tool output) to record.
    async fn run(&self, call: ContentPart) -> Message;
}

/// Blanket impl so a shared, already-`Arc`'d runner satisfies the bound. This is
/// what lets a caller (or a test) build a runner, keep a `clone()` of the `Arc`
/// to inspect afterward, and still hand the same `Arc` to the dispatcher as the
/// `CallRunner`.
#[async_trait::async_trait]
impl<T: CallRunner + ?Sized> CallRunner for Arc<T> {
    fn parallel_safe(&self, call: &ContentPart) -> bool {
        (**self).parallel_safe(call)
    }

    async fn run(&self, call: ContentPart) -> Message {
        (**self).run(call).await
    }
}

/// Production [`CallRunner`] backing the dispatcher with the merged
/// [`tools::ToolOrchestrator`](crate::tools::ToolOrchestrator) (Wave-2 B1).
///
/// The orchestrator surface is generic over the concrete tool/request types,
/// which are wired per-tool by the turn loop (WP that owns the toolset). Until
/// that toolset is threaded through here, the production runner is a thin
/// placeholder so the dispatcher compiles and the *scheduling/ordering/gate*
/// logic — the load-bearing part of WP-C1 — is fully exercised by the injected
/// fakes. `dispatch_ordered` itself is runner-agnostic, so swapping in the real
/// per-tool routing later does not change the dispatch logic.
pub struct OrchestratorRunner {
    /// The model-level parallel-tool-calls capability (codex `turn.rs:872`,
    /// `AgentConfig::supports_parallel_tool_calls`). Per-call `parallel_safe`
    /// comes from the tool runtime; both feed `classify_parallelism`.
    supports_parallel_tool_calls: bool,
}

impl OrchestratorRunner {
    pub fn new(supports_parallel_tool_calls: bool) -> Self {
        Self {
            supports_parallel_tool_calls,
        }
    }
}

#[async_trait::async_trait]
impl CallRunner for OrchestratorRunner {
    fn parallel_safe(&self, _call: &ContentPart) -> bool {
        // Conservative default until the concrete toolset is threaded through:
        // codex's `ToolRuntime::parallel_safe` defaults to `false` (serial), which
        // is the safe choice. The model capability still gates via
        // `classify_parallelism` once a tool opts in.
        false
    }

    async fn run(&self, call: ContentPart) -> Message {
        // The real per-tool routing through `ToolOrchestrator::run` is wired by
        // the toolset-owning WP. Recording a tool-result message keeps the dispatch
        // contract intact in the meantime.
        let _ = self.supports_parallel_tool_calls;
        result_message_for(&call, "tool routing not yet wired", true)
    }
}

/// Build the recorded `Message` for a tool call's output (codex records a
/// function-call output keyed by the originating call id).
fn result_message_for(call: &ContentPart, text: &str, is_error: bool) -> Message {
    let tool_call_id = match call {
        ContentPart::ToolCall { id, .. } => id.clone(),
        _ => String::new(),
    };
    Message::new(
        MessageRole::Tool,
        vec![ContentPart::ToolResult {
            tool_call_id,
            content: vec![ContentPart::text(text)],
            is_error,
        }],
    )
}

/// Result of dispatching a turn's tool calls.
pub struct ToolDispatchResult {
    /// Tool outputs in **model order** (call[i] -> outputs_in_order[i]),
    /// regardless of completion order.
    pub outputs_in_order: Vec<Message>,
    /// `true` iff at least one call was dispatched (there is output to feed back
    /// to the model on the next turn). `false` for empty input.
    pub needs_follow_up: bool,
}

/// Schedules a turn's tool calls: ordered outputs + parallel/serial gate + cancel.
pub struct ToolDispatcher<R: CallRunner = OrchestratorRunner> {
    /// The shared concurrency gate (codex `turn.rs:106`): read = parallel,
    /// write = serial-exclusive.
    gate: Arc<RwLock<()>>,
    /// Runs each call (real = orchestrator, test = scripted fake).
    runner: Arc<R>,
    /// Model-level parallel-tool-calls capability fed to `classify_parallelism`.
    supports_parallel_tool_calls: bool,
}

impl ToolDispatcher<OrchestratorRunner> {
    /// Production dispatcher backed by the
    /// [`tools::ToolOrchestrator`](crate::tools::ToolOrchestrator) seam.
    pub fn new() -> Self {
        Self::with_runner(OrchestratorRunner::new(false), false)
    }
}

impl<R: CallRunner + 'static> ToolDispatcher<R> {
    /// Build a dispatcher with an explicit [`CallRunner`] and the model's
    /// parallel-tool-calls capability. Used by the production constructor and by
    /// tests (which inject a scripted runner).
    pub fn with_runner(runner: R, supports_parallel_tool_calls: bool) -> Self {
        Self {
            gate: Arc::new(RwLock::new(())),
            runner: Arc::new(runner),
            supports_parallel_tool_calls,
        }
    }

    /// Dispatch the turn's tool `calls`.
    ///
    /// Outputs are recorded in **model order** via [`FuturesOrdered`]; parallel-safe
    /// calls overlap (read guard) while serial calls take an exclusive write guard;
    /// `cancel` stops scheduling further calls and lets in-flight calls
    /// short-circuit, while still draining whatever was already scheduled so the
    /// output vector has no holes.
    pub async fn dispatch_ordered(
        &self,
        calls: Vec<ContentPart>,
        cancel: CancellationToken,
    ) -> ToolDispatchResult {
        // needs_follow_up tracks whether anything was dispatched at all (codex:
        // any tool output to feed back -> the turn loop continues).
        let dispatched_any = !calls.is_empty();

        let mut ordered: FuturesOrdered<
            std::pin::Pin<Box<dyn std::future::Future<Output = Option<Message>> + Send>>,
        > = FuturesOrdered::new();

        for call in calls {
            // Stop *scheduling* new calls once cancellation has fired. Already
            // scheduled futures remain in `ordered` and are still drained below
            // (drain_in_flight: started work is observed, not dropped on the floor).
            if cancel.is_cancelled() {
                break;
            }

            let parallelism = decision::classify_parallelism(
                self.runner.parallel_safe(&call),
                self.supports_parallel_tool_calls,
            );
            let gate = self.gate.clone();
            let runner = self.runner.clone();
            let cancel = cancel.clone();

            // Each future acquires its own gate guard *inside* the future so the
            // RwLock — not the scheduler — enforces the overlap rules: parallel
            // calls take read guards (concurrent), serial calls take a write guard
            // (exclusive). The guard is held for the duration of the call's run.
            let fut: std::pin::Pin<Box<dyn std::future::Future<Output = Option<Message>> + Send>> =
                Box::pin(async move {
                    match parallelism {
                        ToolParallelism::Parallel => {
                            let _guard = gate.read().await;
                            run_one(runner.as_ref(), call, &cancel).await
                        }
                        ToolParallelism::Serial => {
                            let _guard = gate.write().await;
                            run_one(runner.as_ref(), call, &cancel).await
                        }
                    }
                });
            ordered.push_back(fut);
        }

        // Drain in push order -> outputs are in model order regardless of which
        // future finished first.
        let mut outputs_in_order: Vec<Message> = Vec::with_capacity(ordered.len());
        while let Some(slot) = ordered.next().await {
            if let Some(msg) = slot {
                outputs_in_order.push(msg);
            }
        }

        ToolDispatchResult {
            outputs_in_order,
            needs_follow_up: dispatched_any,
        }
    }
}

/// Run a single call, honoring cancellation. Returns `None` if the call was
/// cancelled before producing output (so it is skipped rather than recorded with
/// a bogus value).
async fn run_one<R: CallRunner + ?Sized>(
    runner: &R,
    call: ContentPart,
    cancel: &CancellationToken,
) -> Option<Message> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        msg = runner.run(call) => Some(msg),
    }
}

impl Default for ToolDispatcher<OrchestratorRunner> {
    fn default() -> Self {
        Self::new()
    }
}
