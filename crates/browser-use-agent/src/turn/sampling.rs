//! Sampling driver implementation (stream + retry + transport fallback).
//!
//! The concrete [`ModelSamplingDriver`] is the default [`SamplingDriver`]
//! (`turn/mod.rs`) impl over `browser_use_llm::route::ModelClient`. Per turn it:
//!
//! 1. builds an [`LlmRequest`] from the input `Vec<Message>` + a `Route`,
//! 2. calls `ModelClient::stream(route, req)` to obtain an event stream,
//! 3. consumes that stream under a [`CancellationToken`] (via `tokio::select!`
//!    on `cancel.cancelled()` — on cancel it stops and returns
//!    [`AgentError::TurnAborted`]),
//! 4. maps each [`LlmEvent`] to UI events via [`events::map_llm_event`] and emits
//!    them through an injected [`EventSink`],
//! 5. accumulates the assistant message text, tool calls, and [`Usage`], and
//! 6. returns a [`decision::SamplingOutcome`] whose `model_needs_follow_up` is
//!    `true` iff the response carried >=1 tool call.
//!
//! The network send is wrapped in a retry loop that delegates *all* branching to
//! the merged pure core ([`decision::retry_decision`] + [`decision::backoff_ms`]).
//! On a retryable error it sleeps `backoff_ms` then retries up to
//! `stream_max_retries`; on `RetryAction::Fail` it returns the error.
//!
//! ## Codex parity (`codex-rs/core/src/session/turn.rs::run_sampling_request`)
//! - retry budget = `provider.stream_max_retries()` (here `AgentConfig::stream_max_retries`);
//! - `ContextWindowExceeded` / `UsageLimitReached` short-circuit to the matching
//!   [`AgentError`] variant *before* the retry branch (they are never retried);
//! - `model_needs_follow_up = !tool_calls.is_empty()`;
//! - `last_agent_message = (!full_text.is_empty()).then_some(full_text)`;
//! - cancellation mid-stream -> `TurnAborted`;
//! - codex gates retries on the *pre-increment* counter (`if retries < max_retries`),
//!   so `max_retries == N` allows the initial attempt + N retries (N+1 opens). We
//!   pass the pre-increment `attempt` to `decision::retry_decision` to match;
//! - codex computes its delay as `backoff(retries)` *after* `retries += 1`, so the
//!   first retry waits `backoff(1)`. The pure `backoff_ms` is 0-indexed, so we feed
//!   the post-increment `backoff_ms(attempt + 1)` to the decision as the requested
//!   delay — the realized sleep matches codex exactly while the pure budget gate
//!   stays untouched (see `handle_stream_error`).
//!
//! ## Transport switch
//! `RetryAction::SwitchTransport` (codex's WS fallback) is **not yet wired** — the
//! WS transport does not exist in `browser-use-llm` yet. We therefore report
//! `can_switch_transport = false` to the pure decision, which folds the
//! at-max-with-no-transport case straight into `RetryAction::Fail`. When the WS
//! transport lands, set `can_switch_transport` from the live client and handle the
//! returned `SwitchTransport` action here. (TODO: WP that wires WS fallback.)
//!
//! ## Jitter (I/O-layer only)
//! `decision::backoff_ms` is intentionally deterministic so the pure decision stays
//! testable. To match codex's real-world jitter we multiply the *already-decided*
//! delay by a random `0.9..=1.1` factor **here**, strictly behind the actual
//! `tokio::time::sleep`. This never feeds back into the pure decision, so parity of
//! the decision core is preserved. Tests disable jitter (it only affects sleep wall
//! time, never control flow or the returned outcome).

use std::pin::Pin;
use std::sync::Arc;

use browser_use_llm::route::{ModelClient, Route};
use browser_use_llm::schema::{
    FinishReason, LlmError, LlmErrorReason, LlmEvent, LlmRequest, Message, Usage,
};
use futures_util::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::decision::{self, RetryAction, SamplingOutcome};
use crate::events::{self, EventSink, TurnCtx};
use crate::turn::SamplingDriver;
use crate::AgentError;

/// A normalized model event stream: the exact shape [`ModelClient::stream`]
/// yields. Borrows the transport for the duration of one attempt.
pub type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<LlmEvent, LlmError>> + Send + 'a>>;

/// Abstraction over "open one model stream for this request".
///
/// This is the seam that keeps the driver testable without a network: the real
/// transport opens a [`ModelClient`] stream, while tests inject a
/// `ScriptedTransport` that yields canned [`LlmEvent`] sequences (and can fail N
/// times before succeeding, to exercise the retry path).
///
/// The transport yields a *stream* of `Result<LlmEvent, LlmError>` so that both
/// the "open failed" path (codex `stream(&prompt).await` returning `Err`) and the
/// "broke mid-flight" path (a stream item being `Err`) are representable and
/// exercised. Opening the stream is itself fallible.
///
/// `open_stream` takes the request by reference and the returned stream borrows
/// `self` (not `req`), so the caller may reuse `req` across retries.
pub trait SamplingTransport: Send + Sync {
    /// Open a fresh event stream for `req`.
    fn open_stream<'a>(&'a self, req: &LlmRequest) -> Result<EventStream<'a>, LlmError>;
}

/// The real transport: drives a [`ModelClient`] over a fixed [`Route`].
///
/// `ModelClient::stream` is an async fn that returns `Result<Stream, LlmError>`
/// (setup/transport failures surface as the outer `Err`; decode errors surface as
/// `Err` items inside the stream). It owns the per-turn request so the borrowed
/// stream can reference it.
pub struct ModelClientTransport {
    client: Arc<ModelClient>,
    route: Route,
    /// The request is owned here so the stream `ModelClient::stream` produces can
    /// be awaited per attempt; the driver's threaded request is identical.
    req: LlmRequest,
}

impl ModelClientTransport {
    pub fn new(client: Arc<ModelClient>, route: Route, req: LlmRequest) -> Self {
        Self { client, route, req }
    }

    /// Open a stream by awaiting `ModelClient::stream` on the current runtime.
    ///
    /// `ModelClient::stream` is `async`, but [`SamplingTransport::open_stream`] is
    /// sync (so the scripted test transport stays trivial). We bridge with
    /// `block_in_place` + the current handle. This is the only place that blocks,
    /// and it runs on the multi-thread runtime the agent uses.
    fn open_blocking(&self) -> Result<EventStream<'_>, LlmError> {
        let handle = tokio::runtime::Handle::current();
        tokio::task::block_in_place(|| handle.block_on(self.client.stream(&self.route, &self.req)))
    }
}

impl SamplingTransport for ModelClientTransport {
    fn open_stream<'a>(&'a self, _req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        self.open_blocking()
    }
}

/// Does this [`LlmError`] denote a context-window-exceeded condition?
///
/// The schema has no dedicated variant, so we key off the codex-style signal: a
/// non-retryable `InvalidRequest` whose message mentions the context window.
fn is_context_window_exceeded(e: &LlmError) -> bool {
    e.reason == LlmErrorReason::InvalidRequest && {
        let m = e.message.to_ascii_lowercase();
        m.contains("context") && m.contains("window")
    }
}

/// Does this [`LlmError`] denote a hard usage/quota limit (never retried)?
fn is_usage_limit_reached(e: &LlmError) -> bool {
    e.reason == LlmErrorReason::QuotaExceeded
}

/// Map an [`LlmError`] to the agent's [`AgentError`].
///
/// Context-window / usage-limit conditions get dedicated variants (codex
/// short-circuits on these); everything else becomes `Provider(_)`.
fn llm_error_to_agent(e: &LlmError) -> AgentError {
    if is_context_window_exceeded(e) {
        AgentError::ContextWindowExceeded
    } else if is_usage_limit_reached(e) {
        AgentError::UsageLimitReached
    } else {
        AgentError::Provider(e.to_string())
    }
}

/// Server-requested retry delay, if any.
///
/// The structured `Retry-After` hint is carried by `browser-use-llm`'s retry
/// machinery (`Outcome::Fail { retry_after_ms }`), not on the public [`LlmError`]
/// (which deliberately omits it so it never leaks via `Display`). At this layer we
/// therefore have no hint and fall back to the deterministic `backoff_ms`. Kept as
/// a seam for when the executor threads `Retry-After` through to this layer.
fn requested_delay_ms(_e: &LlmError) -> Option<u64> {
    None
}

/// Outcome of one decision-driven retry step.
enum RetryStep {
    /// The error is terminal — return it.
    Fail(AgentError),
    /// Slept for the decided delay; the loop should retry from the top.
    Retry,
}

/// Decide + act on an [`LlmError`] using the PURE decision core.
///
/// `attempt` is the count of retries already performed (0 on the first failure),
/// i.e. exactly codex's `retries` counter *before* it is incremented for this
/// failure. The at-max budget gate therefore uses `*attempt` directly, matching
/// codex's `if retries < max_retries` (pre-increment) — so `max_retries == N`
/// permits the initial attempt plus N retries (N+1 opens total).
///
/// On `RetryAction::Backoff`, this sleeps for the decided delay (with optional
/// I/O-layer jitter), bumps `*attempt`, and returns [`RetryStep::Retry`].
/// `SwitchTransport` is not wired (see module docs) — it is treated as `Fail`.
///
/// ## Backoff index (A1 parity caveat)
/// Codex computes its delay as `backoff(retries)` *after* `retries += 1`, so the
/// first retry waits `backoff(1)`, the second `backoff(2)`, …. The pure
/// `decision::retry_decision`, however, derives its default delay from the same
/// (pre-increment) `retries` it gates on, which would yield `backoff(0)` for the
/// first retry. To reproduce codex's post-increment delay *without* perturbing
/// the pure budget gate, we pass the post-increment `backoff_ms(*attempt + 1)`
/// explicitly as the "server-requested" delay. The decision stays pure and the
/// realized sleep matches codex exactly.
async fn handle_stream_error(
    err: LlmError,
    attempt: &mut u32,
    max_retries: u32,
    jitter: bool,
) -> RetryStep {
    // Context/usage limits never retry — short-circuit to the dedicated variant.
    if is_context_window_exceeded(&err) {
        return RetryStep::Fail(AgentError::ContextWindowExceeded);
    }
    if is_usage_limit_reached(&err) {
        return RetryStep::Fail(AgentError::UsageLimitReached);
    }

    // Prefer a real server `Retry-After` hint; otherwise use codex's
    // post-increment backoff index (see the backoff-index note above).
    let next_attempt = *attempt + 1;
    let requested = requested_delay_ms(&err).or(Some(decision::backoff_ms(next_attempt)));

    // Gate on the PRE-increment count, exactly like codex's `retries < max`.
    let action = decision::retry_decision(
        *attempt,
        max_retries,
        err.retryable,
        // Transport switch (WS fallback) is not wired yet; never offer it.
        false,
        requested,
    );

    match action {
        RetryAction::Fail => RetryStep::Fail(llm_error_to_agent(&err)),
        // Unreachable while can_switch_transport is hard-coded false, but kept
        // explicit so the WS-fallback wiring has an obvious home. Treat as Fail.
        RetryAction::SwitchTransport => RetryStep::Fail(llm_error_to_agent(&err)),
        RetryAction::Backoff { delay_ms } => {
            *attempt = next_attempt;
            let delay = apply_jitter(delay_ms, jitter);
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            RetryStep::Retry
        }
    }
}

/// Apply I/O-layer jitter to an already-decided backoff (see module docs).
///
/// Deterministic decision in, jittered wall-clock delay out. Jitter is a
/// `0.9..=1.1` multiplier; with `jitter == false` (tests) the delay is returned
/// verbatim. Uses a cheap nanosecond-seeded factor — no external `rand` dep — and
/// it only ever perturbs sleep duration, never control flow.
fn apply_jitter(delay_ms: u64, jitter: bool) -> u64 {
    if !jitter || delay_ms == 0 {
        return delay_ms;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Map nanos -> [0.9, 1.1].
    let frac = (nanos % 1000) as f64 / 1000.0; // [0.0, 1.0)
    let factor = 0.9 + 0.2 * frac;
    (delay_ms as f64 * factor) as u64
}

/// Default [`SamplingDriver`]: stream + retry over the pure decision core.
pub struct ModelSamplingDriver<T: SamplingTransport> {
    transport: T,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    /// Retry budget (codex `provider.stream_max_retries()`).
    max_retries: u32,
    /// Whether to apply I/O-layer jitter to the post-decision backoff sleep.
    jitter: bool,
}

impl<T: SamplingTransport> ModelSamplingDriver<T> {
    /// Build a driver. `ctx` carries session/model identity for emitted events.
    /// The transport owns the per-turn [`LlmRequest`] / [`Route`] (see
    /// [`ModelClientTransport`]); the driver only threads the input through.
    pub fn new(transport: T, sink: Arc<dyn EventSink>, ctx: TurnCtx, max_retries: u32) -> Self {
        Self {
            transport,
            sink,
            ctx,
            max_retries,
            jitter: true,
        }
    }

    /// Disable I/O-layer jitter (deterministic sleeps). Used by tests.
    pub fn without_jitter(mut self) -> Self {
        self.jitter = false;
        self
    }

    /// Map an [`LlmEvent`] to UI events and emit them through the sink.
    fn emit_event(&self, ev: &LlmEvent) {
        for pending in events::map_llm_event(&self.ctx, ev) {
            self.sink.emit(pending);
        }
    }

    /// Fold a single successfully-decoded event into the accumulator + sink.
    fn consume_event(&self, acc: &mut TurnAccumulator, ev: LlmEvent) -> StreamProgress {
        // Emit UI events first (map is pure; emit is the only side effect).
        self.emit_event(&ev);
        match ev {
            LlmEvent::TextDelta { delta, .. } => {
                acc.full_text.push_str(&delta);
                StreamProgress::Continue
            }
            LlmEvent::ToolCall { .. } => {
                acc.tool_call_count += 1;
                StreamProgress::Continue
            }
            LlmEvent::Finish {
                usage,
                finish_reason,
            } => {
                acc.usage = Some(usage);
                acc.finish_reason = finish_reason;
                StreamProgress::Done
            }
            // Reasoning, lifecycle markers, provider-side notices, step finishes:
            // no accumulation; their UI mapping (if any) already happened above.
            _ => StreamProgress::Continue,
        }
    }
}

/// Accumulated state for one assistant turn, threaded through the stream loop.
#[derive(Default)]
struct TurnAccumulator {
    full_text: String,
    tool_call_count: usize,
    #[allow(dead_code)]
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
}

/// What consuming one streamed event told the loop to do.
enum StreamProgress {
    /// Keep pulling events.
    Continue,
    /// The stream ended (`Finish`) — assemble the outcome.
    Done,
}

impl<T: SamplingTransport + 'static> SamplingDriver for ModelSamplingDriver<T> {
    async fn run_sampling_request(
        &self,
        input: Vec<Message>,
        cancel: CancellationToken,
    ) -> Result<SamplingOutcome, AgentError> {
        // `attempt` == retries already performed (codex `retries`). The outer
        // loop re-opens the stream on each retryable failure. The transport owns
        // the actual request; we thread the input through for parity / future use.
        let req = build_request(&self.ctx, input);
        let mut attempt: u32 = 0;
        loop {
            // ---- open the stream (codex: `client.stream(&prompt).await`) ----
            let mut stream = match self.transport.open_stream(&req) {
                Ok(s) => s,
                Err(e) => {
                    match handle_stream_error(e, &mut attempt, self.max_retries, self.jitter).await
                    {
                        RetryStep::Retry => continue,
                        RetryStep::Fail(err) => return Err(err),
                    }
                }
            };

            // ---- consume the stream under the cancellation token ----
            let mut acc = TurnAccumulator::default();
            // Set when a retryable mid-stream error tells us to restart the outer
            // loop (codex breaks the inner loop then re-opens).
            let mut restart = false;

            loop {
                let maybe_event = tokio::select! {
                    _ = cancel.cancelled() => {
                        return Err(AgentError::TurnAborted);
                    }
                    ev = stream.next() => ev,
                };

                match maybe_event {
                    Some(Ok(ev)) => match self.consume_event(&mut acc, ev) {
                        StreamProgress::Continue => {}
                        StreamProgress::Done => break,
                    },
                    Some(Err(e)) => {
                        match handle_stream_error(e, &mut attempt, self.max_retries, self.jitter)
                            .await
                        {
                            RetryStep::Retry => {
                                restart = true;
                                break;
                            }
                            RetryStep::Fail(err) => return Err(err),
                        }
                    }
                    None => break,
                }
            }

            if restart {
                continue;
            }

            // ---- assemble the outcome (codex parity) ----
            let model_needs_follow_up = decision::needs_follow_up(acc.tool_call_count >= 1, false);
            return Ok(SamplingOutcome {
                model_needs_follow_up,
                last_agent_message: (!acc.full_text.is_empty()).then_some(acc.full_text),
                finish_reason: acc.finish_reason,
            });
        }
    }
}

/// Build the per-turn [`LlmRequest`] from the input messages + turn identity.
///
/// The model / provider come from the [`TurnCtx`]; tool schema and sampling knobs
/// are owned by the transport (the real transport fixes the full request at
/// construction). Kept as a free fn so it is unit-reachable.
fn build_request(ctx: &TurnCtx, input: Vec<Message>) -> LlmRequest {
    let mut req = LlmRequest::new(ctx.model.clone(), ctx.provider.clone());
    req.messages = input;
    req
}
