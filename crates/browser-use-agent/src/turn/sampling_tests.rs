//! Tests for the async [`ModelSamplingDriver`] (WP-B5).
//!
//! NETWORK-FREE: every test injects a `ScriptedTransport` that returns canned
//! [`LlmEvent`] sequences (and can fail N times before succeeding). No
//! `ModelClient` / no real socket is ever touched. Jitter is disabled via
//! `ModelSamplingDriver::without_jitter` so backoff sleeps are deterministic;
//! the retry tests therefore sleep the exact `backoff_ms` values (a few hundred
//! ms total — `tokio`'s `test-util`/`start_paused` is intentionally not required
//! so no Cargo manifest change is needed).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use browser_use_llm::schema::{
    ContentPart, FinishReason, LlmError, LlmErrorReason, LlmEvent, LlmRequest, Message,
    MessageRole, Usage,
};
use futures_util::stream;
use tokio_util::sync::CancellationToken;

use crate::events::names;
use crate::events::{EventSink, TurnCtx};
use crate::testkit::RecordingSink;
use crate::turn::sampling::{EventStream, ModelSamplingDriver, SamplingTransport};
use crate::turn::SamplingDriver;
use crate::AgentError;

// ---- scripted transport ---------------------------------------------------

/// One open attempt's result: either the open itself fails (codex
/// `stream().await -> Err`), or it yields a stream of items (each of which may be
/// an `Err`, modeling a mid-flight break).
enum OpenScript {
    OpenErr(LlmError),
    Stream(Vec<Result<LlmEvent, LlmError>>),
}

/// A transport that replays a queue of [`OpenScript`]s, one per `open_stream`
/// call. The shared `opens` counter lets a test assert how many opens (retries +
/// the final success) happened; a `Clone`able `Arc` handle to it is returned so
/// the test can read it after the driver consumes the transport.
struct ScriptedTransport {
    scripts: Vec<OpenScript>,
    next: AtomicUsize,
    opens: Arc<AtomicUsize>,
}

impl ScriptedTransport {
    fn new(scripts: Vec<OpenScript>) -> (Self, Arc<AtomicUsize>) {
        let opens = Arc::new(AtomicUsize::new(0));
        (
            Self {
                scripts,
                next: AtomicUsize::new(0),
                opens: opens.clone(),
            },
            opens,
        )
    }
}

impl SamplingTransport for ScriptedTransport {
    fn open_stream<'a>(&'a self, _req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let idx = self.next.fetch_add(1, Ordering::SeqCst);
        // Past the end of the queue, reuse the last script (defensive).
        let script = self.scripts.get(idx).or_else(|| self.scripts.last());
        match script {
            Some(OpenScript::OpenErr(e)) => Err(e.clone()),
            Some(OpenScript::Stream(items)) => Ok(Box::pin(stream::iter(items.clone()))),
            None => Ok(Box::pin(stream::empty())),
        }
    }
}

/// A transport whose stream never yields (so only cancellation can end the loop).
struct PendingTransport;

impl SamplingTransport for PendingTransport {
    fn open_stream<'a>(&'a self, _req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        Ok(Box::pin(stream::pending()))
    }
}

// ---- helpers --------------------------------------------------------------

fn ctx() -> TurnCtx {
    TurnCtx {
        session_id: "sess-1".to_string(),
        model: "gpt-5-codex".to_string(),
        provider: "openai".to_string(),
        turn_idx: 0,
        attempt: 0,
    }
}

fn driver(
    transport: ScriptedTransport,
    sink: Arc<RecordingSink>,
    max_retries: u32,
) -> ModelSamplingDriver<ScriptedTransport> {
    let sink: Arc<dyn EventSink> = sink;
    ModelSamplingDriver::new(transport, sink, ctx(), max_retries).without_jitter()
}

fn user_input() -> Vec<Message> {
    vec![Message::new(
        MessageRole::User,
        vec![ContentPart::text("hello")],
    )]
}

fn text_delta(s: &str) -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::TextDelta {
        id: "t0".to_string(),
        delta: s.to_string(),
    })
}

fn tool_call(name: &str) -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::ToolCall {
        id: "call-1".to_string(),
        name: name.to_string(),
        input: serde_json::json!({"arg": 1}),
    })
}

fn finish(reason: FinishReason) -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::Finish {
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        },
        finish_reason: Some(reason),
    })
}

/// A retryable transport-level error (codex `is_retryable() == true`).
fn retryable_err(msg: &str) -> LlmError {
    let e = LlmError::new(LlmErrorReason::Transport, msg);
    assert!(e.retryable, "Transport errors must be retryable");
    e
}

// ---- (1) text deltas + tool call -> follow_up + emitted events ------------

#[tokio::test]
async fn tool_call_sets_follow_up_and_records_message_and_events() {
    let (transport, _opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        text_delta("Let me "),
        text_delta("look that up."),
        tool_call("search"),
        finish(FinishReason::ToolUse),
    ])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert!(
        out.model_needs_follow_up,
        ">=1 tool call must set model_needs_follow_up"
    );
    assert_eq!(
        out.last_agent_message.as_deref(),
        Some("Let me look that up."),
        "assistant text deltas must be concatenated into last_agent_message"
    );
    assert_eq!(out.finish_reason, Some(FinishReason::ToolUse));

    // Events were emitted to the sink: two stream deltas, a tool.started, and a
    // terminal token_count.
    let events = sink.drain();
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            names::MODEL_STREAM_DELTA,
            names::MODEL_STREAM_DELTA,
            names::TOOL_STARTED,
            names::TOKEN_COUNT,
        ],
        "mapped UI events must be emitted in stream order"
    );
    // The tool.started payload carries the tool name.
    let tool_started = &events[2];
    assert_eq!(tool_started.payload["name"], serde_json::json!("search"));
}

// ---- (2) text-only stream -> no follow_up ---------------------------------

#[tokio::test]
async fn text_only_stream_does_not_request_follow_up() {
    let (transport, _opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        text_delta("All done."),
        finish(FinishReason::Stop),
    ])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert!(
        !out.model_needs_follow_up,
        "no tool call -> model_needs_follow_up must be false"
    );
    assert_eq!(out.last_agent_message.as_deref(), Some("All done."));
    assert_eq!(out.finish_reason, Some(FinishReason::Stop));

    let types: Vec<String> = sink.drain().iter().map(|e| e.event_type.clone()).collect();
    assert!(
        !types.iter().any(|t| t == names::TOOL_STARTED),
        "no tool.started event for a text-only turn"
    );
}

// ---- (3) retryable failures then success ----------------------------------

#[tokio::test]
async fn retries_after_retryable_failures_then_succeeds() {
    // The first two opens fail with a retryable transport error; the third serves
    // a successful script. With max_retries == 5 the driver must retry twice and
    // then succeed. Jitter is off, so the two sleeps are backoff_ms(1)+backoff_ms(2)
    // == 400ms + 800ms (sub-second, deterministic).
    let (transport, opens) = ScriptedTransport::new(vec![
        OpenScript::OpenErr(retryable_err("boom 1")),
        OpenScript::OpenErr(retryable_err("boom 2")),
        OpenScript::Stream(vec![text_delta("recovered"), finish(FinishReason::Stop)]),
    ]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("driver should recover after retryable failures");

    assert_eq!(out.last_agent_message.as_deref(), Some("recovered"));
    // 2 failed opens + 1 successful open == 3 total open attempts.
    assert_eq!(
        opens.load(Ordering::SeqCst),
        3,
        "driver must retry the two failures then succeed on the third open"
    );
}

// ---- (3b) retries are bounded by max_retries; non-retryable fails fast -----

#[tokio::test]
async fn retries_are_bounded_by_max_retries() {
    // Every open fails retryably and max_retries == 2, so the driver makes the
    // initial attempt + 2 retries == 3 opens, then returns the provider error.
    // Jitter off: the two sleeps are backoff_ms(1)+backoff_ms(2) (sub-second).
    let (transport, opens) =
        ScriptedTransport::new(vec![OpenScript::OpenErr(retryable_err("always down"))]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink, 2);

    let err = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect_err("exhausting the retry budget must fail");
    assert!(
        matches!(err, AgentError::Provider(_)),
        "exhausted retries surface the provider error, got {err:?}"
    );
    assert_eq!(
        opens.load(Ordering::SeqCst),
        3,
        "initial attempt + max_retries(2) == 3 opens"
    );
}

#[tokio::test]
async fn non_retryable_error_fails_without_retrying() {
    // An InvalidRequest is not retryable: one open, immediate failure.
    let (transport, opens) = ScriptedTransport::new(vec![OpenScript::OpenErr(LlmError::new(
        LlmErrorReason::InvalidRequest,
        "bad request",
    ))]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink, 5);

    let err = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect_err("non-retryable error must fail");
    assert!(matches!(err, AgentError::Provider(_)), "got {err:?}");
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "non-retryable errors must not be retried"
    );
}

// ---- (4) cancellation mid-stream -> TurnAborted ---------------------------

#[tokio::test]
async fn cancellation_mid_stream_returns_turn_aborted() {
    // A never-ending stream: the cancel branch of the select! must win.
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let d = ModelSamplingDriver::new(PendingTransport, sink, ctx(), 5).without_jitter();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    // Cancel shortly after the request starts consuming.
    tokio::spawn(async move {
        cancel2.cancel();
    });

    let err = d
        .run_sampling_request(user_input(), cancel)
        .await
        .expect_err("cancellation must abort the turn");
    assert!(
        matches!(err, AgentError::TurnAborted),
        "cancelled turn must return AgentError::TurnAborted, got {err:?}"
    );
}
