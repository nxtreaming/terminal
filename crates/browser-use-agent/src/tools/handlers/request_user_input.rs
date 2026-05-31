//! `request_user_input` tool: the human-in-the-loop tool the model calls to ask
//! the user one to three short questions (each with mutually-exclusive options)
//! and wait for a structured response.
//!
//! This is the async re-implementation of codex's `request_user_input` over our
//! merged [`ToolRuntime`](crate::tools::runtime::ToolRuntime) seam. It implements
//! the full trait stack ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it
//! can be driven by the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator),
//! mirroring the `update_plan` tool's structure (`tools/handlers/update_plan.rs`),
//! which is the closest analog: a non-FS, validate-and-return tool.
//!
//! # Host round-trip via a pluggable [`RequestUserInputResponder`]
//!
//! In codex the handler does NOT compute the answer itself: it normalizes the
//! args and calls `session.request_user_input(turn, call_id, args).await`, which
//! BLOCKS until the UI/host returns a [`RequestUserInputResponse`]
//! (codex `core/src/tools/handlers/request_user_input.rs:65-87`). The legacy impl
//! does the same: it appends a [`REQUEST_USER_INPUT_REQUEST_EVENT`] and then
//! `wait_for_request_user_input_response` blocks on the store for the matching
//! [`REQUEST_USER_INPUT_RESPONSE_EVENT`]
//! (`browser-use-core/src/request_user_input.rs:141-163`). That prompt→response
//! round-trip lives in the session/host layer.
//!
//! The new `browser-use-agent` engine has no answer-channel seam on `ToolCtx` /
//! `TurnEnv` (those carry only `call_id`/`tool_name`/`cwd` and the sandbox /
//! network / guardian flags), and threading one through the orchestrator's `run`
//! would require editing files this WP does not own (`entrypoint/mod.rs`, the
//! `ToolCtx` struct). So the round-trip is modeled with a mechanism this WP DOES
//! own: the tool holds a pluggable [`RequestUserInputResponder`]. `run`
//! [validates](validate_request_user_input) + normalizes the args (faithful to
//! codex's `normalize_request_user_input_args`,
//! `core/src/tools/handlers/request_user_input_spec.rs:99-115`, forcing
//! `is_other = true` on every question), then AWAITS the responder for the user's
//! [`RequestUserInputResponse`] and returns the serialized ANSWERS (prefixed with
//! [`REQUEST_USER_INPUT_STDOUT_PREFIX`]) — the codex/legacy behavior of returning
//! the answer to the model, not the request masquerading as an answer.
//!
//! The default responder ([`EchoAutoResponder`]) is a deterministic auto-answer
//! (the first option of each question), keeping tests network/host-free while
//! still exercising the request→answer round-trip. A real host injects its own
//! responder via [`RequestUserInputTool::with_responder`] (the
//! `build_tool_dispatcher` seam).
//!
//! CROSS-FILE NOTE: a *real* blocking-on-the-human round-trip still needs the
//! entrypoint (`entrypoint/mod.rs`) to construct + inject a responder backed by
//! the host's UI channel (the [`REQUEST_USER_INPUT_REQUEST_EVENT`] /
//! [`REQUEST_USER_INPUT_RESPONSE_EVENT`] events). The codex root-thread gate
//! ("request_user_input can only be used by the root thread",
//! `request_user_input.rs:54-58`) and the mode-availability gate
//! (`request_user_input_unavailable_message`) are likewise session-layer concerns
//! the entrypoint owns. This WP provides the responder seam + a default
//! auto-responder; wiring the host's real responder is the entrypoint's job.
//!
//! # Parity grounding (file:line)
//!
//! Codex sources under `/home/exedev/repos/codex/codex-rs`:
//! * **Args / wire shape** — `RequestUserInputArgs { questions:
//!   Vec<RequestUserInputQuestion> }`; `RequestUserInputQuestion { id, header,
//!   question, is_other (rename `"isOther"`, `#[serde(default)]`), is_secret
//!   (rename `"isSecret"`, `#[serde(default)]`), options:
//!   Option<Vec<RequestUserInputQuestionOption>> (`skip_serializing_if
//!   Option::is_none`) }`; `RequestUserInputQuestionOption { label, description }`
//!   (`protocol/src/request_user_input.rs:8-34`). Our types mirror these
//!   field-for-field including the camelCase serde renames and the `default` /
//!   `skip_serializing_if` attributes, so the wire shape is byte-identical.
//! * **Response / wire shape** — `RequestUserInputResponse { answers:
//!   HashMap<String, RequestUserInputAnswer> }` keyed by the question `id`, where
//!   `RequestUserInputAnswer { answers: Vec<String> }`
//!   (`protocol/src/request_user_input.rs:36-44`). Defined here for the deferred
//!   host round-trip and to round-trip-test the exact wire shape.
//! * **Validation** — codex `normalize_request_user_input_args`
//!   (`core/src/tools/handlers/request_user_input_spec.rs:99-115`; legacy
//!   `browser-use-core/src/request_user_input.rs:51-66`, byte-identical): rejects
//!   when ANY question has missing or empty `options` ("requires non-empty options
//!   for every question") and forces `is_other = true` on every question. We
//!   reproduce that exactly, and additionally reject an empty `questions` list
//!   (codex marks `questions` required + "Prefer 1 and do not exceed 3",
//!   `request_user_input_spec.rs:55-69`) and blank `id`/`question` text.
//! * **Tool name** — `REQUEST_USER_INPUT_TOOL_NAME = "request_user_input"`
//!   (`request_user_input_spec.rs:8`); identical to legacy
//!   `browser-use-core/src/request_user_input.rs` (`REQUEST_USER_INPUT_TOOL_NAME`).
//!
//! LEGACY cross-ref (`/home/exedev/new-core/terminal-decodex/crates/browser-use-core`):
//! the carved `request_user_input.rs` module reproduces the identical
//! `RequestUserInput*` structs and uses the tool name / event consts
//! `REQUEST_USER_INPUT_TOOL_NAME`, `REQUEST_USER_INPUT_REQUEST_EVENT`,
//! `REQUEST_USER_INPUT_RESPONSE_EVENT` (`lib.rs:19696`, `:29007`). We re-export
//! the same event-type consts so the deferred host integration can key on the
//! legacy event shapes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::session::{SessionId, SharedStore};
use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// Durable control-channel event name appended when the model asks the operator
/// for input. The TUI consumes this to render the prompt to the human.
///
/// This is the DOTTED name the TUI keys on
/// (`browser-use-tui/src/main.rs`: `REQUEST_USER_INPUT_REQUEST_EVENT =
/// "request_user_input.requested"`), distinct from the legacy underscore-form
/// [`REQUEST_USER_INPUT_REQUEST_EVENT`] kept above for the legacy carve.
pub const REQUEST_USER_INPUT_REQUESTED_EVENT: &str = "request_user_input.requested";

/// Durable control-channel event name the operator's answer is delivered on.
///
/// Matches the TUI's `REQUEST_USER_INPUT_RESPONSE_EVENT =
/// "request_user_input.response"`. The [`StoreRoundTripResponder`] waits for
/// this event and parses the answers from its payload.
pub const REQUEST_USER_INPUT_RESPONSE_DOTTED_EVENT: &str = "request_user_input.response";

/// Payload key the request event carries the (validated, normalized) questions
/// under. Mirrors the TUI's `REQUEST_USER_INPUT_REQUEST_KEY`.
pub const REQUEST_USER_INPUT_REQUEST_KEY: &str = "request_user_input_request";

/// How often the store round-trip responder polls for a response event.
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The tool name surfaced to the model.
///
/// Codex parity: `REQUEST_USER_INPUT_TOOL_NAME`
/// (`core/src/tools/handlers/request_user_input_spec.rs:8`); identical to legacy
/// `browser-use-core/src/request_user_input.rs`.
pub const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";

/// Event type emitted when the tool requests user input (host-bound).
///
/// Legacy parity: `REQUEST_USER_INPUT_REQUEST_EVENT` (the legacy carve appends
/// this event then blocks, `browser-use-core/src/request_user_input.rs:120`,
/// `lib.rs:29004`). Carried here so the deferred host round-trip can key on the
/// same event shape.
pub const REQUEST_USER_INPUT_REQUEST_EVENT: &str = "request_user_input_request";

/// Event type the host sends back with the user's answers.
///
/// Legacy parity: `REQUEST_USER_INPUT_RESPONSE_EVENT`
/// (`browser-use-core/src/request_user_input.rs:120`, `lib.rs:29007`).
pub const REQUEST_USER_INPUT_RESPONSE_EVENT: &str = "request_user_input_response";

/// Prefix on the [`ExecOutput::stdout`] JSON payload so a later host-aware layer
/// can recognize the serialized request and complete the round-trip.
///
/// This is a property of our [`ExecOutput`] fallback seam, NOT a codex/legacy
/// wire constant (the request side of this WP returns the request, not the
/// answer — see the module-doc "host round-trip is DEFERRED" note).
pub const REQUEST_USER_INPUT_STDOUT_PREFIX: &str = "request_user_input:";

/// A selectable option for a question.
///
/// Codex parity: `RequestUserInputQuestionOption { label, description }`
/// (`protocol/src/request_user_input.rs:8-12`); identical to legacy
/// `RequestUserInputOption` (`browser-use-core/src/request_user_input.rs:18-22`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserInputOption {
    /// User-facing label (1-5 words).
    pub label: String,
    /// One short sentence explaining the impact/tradeoff if selected.
    pub description: String,
}

/// A single question shown to the user.
///
/// Codex parity: `RequestUserInputQuestion { id, header, question, is_other,
/// is_secret, options }` (`protocol/src/request_user_input.rs:14-29`); identical
/// to legacy `browser-use-core/src/request_user_input.rs:24-34`. The serde
/// attributes are reproduced byte-for-byte so the wire shape matches:
/// * `is_other` / `is_secret` use the camelCase wire names `"isOther"` /
///   `"isSecret"` and are `#[serde(default)]` (so they may be omitted on input).
/// * `options` is `#[serde(skip_serializing_if = "Option::is_none")]` (omitted
///   when `None`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserInputQuestion {
    /// Stable identifier for mapping answers (snake_case).
    pub id: String,
    /// Short header label shown in the UI (12 or fewer chars).
    pub header: String,
    /// Single-sentence prompt shown to the user.
    pub question: String,
    /// Whether the client should add a free-form "Other" option. Codex/legacy
    /// `normalize` force this to `true` on every question. Wire name `"isOther"`.
    #[serde(rename = "isOther", default)]
    pub is_other: bool,
    /// Whether the answer is a secret (masked input). Wire name `"isSecret"`.
    #[serde(rename = "isSecret", default)]
    pub is_secret: bool,
    /// The mutually-exclusive choices. Codex's `normalize` requires this to be
    /// `Some` and non-empty for every question. Omitted on the wire when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<UserInputOption>>,
}

/// Arguments the model passes to the `request_user_input` tool.
///
/// Codex parity: `RequestUserInputArgs { questions:
/// Vec<RequestUserInputQuestion> }` (`protocol/src/request_user_input.rs:31-34`);
/// identical to legacy `browser-use-core/src/request_user_input.rs:36-39`. This is
/// the typed request for the [`RequestUserInputTool`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestUserInputRequest {
    /// The questions to show the user (codex: "Prefer 1 and do not exceed 3").
    pub questions: Vec<UserInputQuestion>,
}

/// The answers to a single question (host-supplied).
///
/// Codex parity: `RequestUserInputAnswer { answers: Vec<String> }`
/// (`protocol/src/request_user_input.rs:36-39`); identical to legacy
/// `browser-use-core/src/request_user_input.rs:41-44`. A list because a question
/// may be multi-select; a freeform "Other" answer is carried as a string here.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserInputAnswer {
    /// The selected option labels (or freeform text) for the question.
    pub answers: Vec<String>,
}

/// The response the host returns with the user's answers, keyed by question `id`.
///
/// Codex parity: `RequestUserInputResponse { answers: HashMap<String,
/// RequestUserInputAnswer> }` (`protocol/src/request_user_input.rs:41-44`);
/// identical to legacy `browser-use-core/src/request_user_input.rs:46-49`.
/// Surfacing this answer to the model is the DEFERRED host round-trip (see the
/// module doc).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestUserInputResponse {
    /// Map of question `id` -> the answers for that question.
    pub answers: HashMap<String, UserInputAnswer>,
}

impl RequestUserInputRequest {
    /// Convenience constructor for a single question with options, mirroring the
    /// common one-question case codex prefers (`request_user_input_spec.rs:66`).
    pub fn single<I, O>(
        id: impl Into<String>,
        header: impl Into<String>,
        question: impl Into<String>,
        options: I,
    ) -> Self
    where
        I: IntoIterator<Item = O>,
        O: Into<UserInputOption>,
    {
        Self {
            questions: vec![UserInputQuestion {
                id: id.into(),
                header: header.into(),
                question: question.into(),
                is_other: false,
                is_secret: false,
                options: Some(options.into_iter().map(Into::into).collect()),
            }],
        }
    }
}

impl From<(&str, &str)> for UserInputOption {
    fn from((label, description): (&str, &str)) -> Self {
        UserInputOption {
            label: label.to_string(),
            description: description.to_string(),
        }
    }
}

/// Validate + normalize a `request_user_input` request against codex's rules.
///
/// Codex parity: `normalize_request_user_input_args`
/// (`core/src/tools/handlers/request_user_input_spec.rs:99-115`; legacy
/// `browser-use-core/src/request_user_input.rs:51-66`, byte-identical):
/// * Rejects when ANY question has missing or empty `options` — error contains
///   "non-empty options" (verbatim codex/legacy message reproduced).
/// * Forces `is_other = true` on every question.
///
/// We additionally reject (codex marks `questions` required and each question's
/// `id` / `question` non-empty via the schema, `request_user_input_spec.rs:33-69`):
/// * an empty `questions` list;
/// * a blank `id` or `question` on any question.
///
/// Returns the normalized request (with `is_other` forced true) on success.
pub fn validate_request_user_input(
    mut req: RequestUserInputRequest,
) -> Result<RequestUserInputRequest, ToolError> {
    if req.questions.is_empty() {
        return Err(ToolError::Rejected(
            "request_user_input requires at least one question".to_string(),
        ));
    }

    for (idx, question) in req.questions.iter().enumerate() {
        if question.id.trim().is_empty() {
            return Err(ToolError::Rejected(format!(
                "request_user_input: question {} has an empty id",
                idx + 1
            )));
        }
        if question.question.trim().is_empty() {
            return Err(ToolError::Rejected(format!(
                "request_user_input: question {} has empty question text",
                idx + 1
            )));
        }
    }

    // Codex/legacy `normalize_request_user_input_args`: every question must have
    // non-empty options. The error string mirrors codex/legacy verbatim
    // (`request_user_input_spec.rs:107`; legacy `request_user_input.rs:59`).
    let missing_options = req
        .questions
        .iter()
        .any(|q| q.options.as_ref().is_none_or(Vec::is_empty));
    if missing_options {
        return Err(ToolError::Rejected(
            "request_user_input requires non-empty options for every question".to_string(),
        ));
    }

    // Codex/legacy force `is_other = true` on every question
    // (`request_user_input_spec.rs:110-112`; legacy `request_user_input.rs:62-64`):
    // the client always adds a free-form "Other" option.
    for question in &mut req.questions {
        question.is_other = true;
    }

    Ok(req)
}

/// The host's answer channel for the `request_user_input` round-trip.
///
/// Codex parity: `session.request_user_input(...).await` (codex
/// `core/src/tools/handlers/request_user_input.rs:65-87`) and the legacy
/// `wait_for_request_user_input_response`
/// (`browser-use-core/src/request_user_input.rs:141-163`): surface the questions
/// to the UI/host and block for the user's [`RequestUserInputResponse`]. This
/// trait is the engine's seam for that — the entrypoint injects a real host-backed
/// responder; tests / offline runs use the default [`EchoAutoResponder`].
#[async_trait::async_trait]
pub trait RequestUserInputResponder: Send + Sync {
    /// Surface the (already-validated, normalized) `request` to the host and
    /// AWAIT the user's answers. The returned [`RequestUserInputResponse`] is keyed
    /// by each question's `id`.
    async fn respond(
        &self,
        request: &RequestUserInputRequest,
    ) -> Result<RequestUserInputResponse, ToolError>;
}

/// The default responder: a deterministic auto-answer that selects the FIRST
/// option of each question.
///
/// This keeps tests / offline runs network- and host-free while still exercising
/// the real request→answer round-trip (the tool returns ANSWERS, not the request).
/// A real host replaces this via [`RequestUserInputTool::with_responder`].
#[derive(Clone, Debug, Default)]
pub struct EchoAutoResponder;

#[async_trait::async_trait]
impl RequestUserInputResponder for EchoAutoResponder {
    async fn respond(
        &self,
        request: &RequestUserInputRequest,
    ) -> Result<RequestUserInputResponse, ToolError> {
        // Auto-select the first option of each question (the request is already
        // validated to have non-empty options for every question).
        let mut answers = HashMap::new();
        for question in &request.questions {
            let first = question
                .options
                .as_ref()
                .and_then(|opts| opts.first())
                .map(|opt| opt.label.clone())
                .unwrap_or_default();
            answers.insert(
                question.id.clone(),
                UserInputAnswer {
                    answers: vec![first],
                },
            );
        }
        Ok(RequestUserInputResponse { answers })
    }
}

/// Production responder that round-trips through the durable session store.
///
/// On [`respond`](RequestUserInputResponder::respond) it appends a
/// [`REQUEST_USER_INPUT_REQUESTED_EVENT`] event carrying the questions, then
/// waits for a matching [`REQUEST_USER_INPUT_RESPONSE_DOTTED_EVENT`] event
/// (appended by the TUI / operator) and returns the answers it carries — exactly
/// the codex/legacy `request_user_input` round-trip
/// (`browser-use-core/src/request_user_input.rs:141-163`), now over the new
/// engine's [`SharedStore`].
///
/// The store exposes no async change notification at this layer, so this polls
/// [`Store::events_for_session`](browser_use_store::Store::events_for_session) at
/// [`RESPONSE_POLL_INTERVAL`]. Each blocking store read runs on
/// [`spawn_blocking`](tokio::task::spawn_blocking) so the async runtime is never
/// stalled and the store mutex is never held across an `.await`. An optional
/// timeout bounds the wait; `None` waits indefinitely (the operator may take any
/// amount of time to answer, matching the legacy blocking behavior).
pub struct StoreRoundTripResponder {
    store: SharedStore,
    session_id: SessionId,
    timeout: Option<Duration>,
}

impl StoreRoundTripResponder {
    /// Build a responder that waits indefinitely for the operator's answer.
    pub fn new(store: SharedStore, session_id: SessionId) -> Self {
        Self {
            store,
            session_id,
            timeout: None,
        }
    }

    /// Build a responder that gives up after `timeout` without an answer.
    pub fn with_timeout(store: SharedStore, session_id: SessionId, timeout: Duration) -> Self {
        Self {
            store,
            session_id,
            timeout: Some(timeout),
        }
    }

    /// Snapshot the session's events (blocking store read, off the async runtime).
    async fn read_events(&self) -> Result<Vec<browser_use_protocol::EventRecord>, ToolError> {
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.as_str().to_string();
        tokio::task::spawn_blocking(move || {
            let store = store.lock().map_err(|_| {
                ToolError::Other(anyhow::anyhow!("request_user_input: store mutex poisoned"))
            })?;
            store
                .events_for_session(&session_id)
                .map_err(|e| ToolError::Other(anyhow::anyhow!(e)))
        })
        .await
        .map_err(|e| {
            ToolError::Other(anyhow::anyhow!("request_user_input: store read task: {e}"))
        })?
    }

    /// Append the request event (blocking store write, off the async runtime).
    async fn append_request(&self, payload: serde_json::Value) -> Result<(), ToolError> {
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.as_str().to_string();
        tokio::task::spawn_blocking(move || {
            let store = store.lock().map_err(|_| {
                ToolError::Other(anyhow::anyhow!("request_user_input: store mutex poisoned"))
            })?;
            store
                .append_event(&session_id, REQUEST_USER_INPUT_REQUESTED_EVENT, payload)
                .map(|_| ())
                .map_err(|e| ToolError::Other(anyhow::anyhow!(e)))
        })
        .await
        .map_err(|e| {
            ToolError::Other(anyhow::anyhow!("request_user_input: store write task: {e}"))
        })?
    }
}

#[async_trait::async_trait]
impl RequestUserInputResponder for StoreRoundTripResponder {
    async fn respond(
        &self,
        request: &RequestUserInputRequest,
    ) -> Result<RequestUserInputResponse, ToolError> {
        // Snapshot the log length first so we only ever match a response that
        // arrives AFTER this request, never a stale one from a prior round-trip.
        let baseline = self.read_events().await?.len();

        // Announce the request on the durable control channel for the TUI/operator.
        // Carry the questions both under the TUI's key and inline, so either
        // consumer shape can render them.
        let questions = serde_json::to_value(&request.questions)
            .map_err(|e| ToolError::Other(anyhow::anyhow!(e)))?;
        let payload = serde_json::json!({
            REQUEST_USER_INPUT_REQUEST_KEY: { "questions": questions.clone() },
            "questions": questions,
        });
        self.append_request(payload).await?;

        // Wait for the operator's response event.
        let deadline = self.timeout.map(|t| std::time::Instant::now() + t);
        loop {
            let events = self.read_events().await?;
            if let Some(answers) = find_response(&events, baseline) {
                return Ok(answers);
            }
            if let Some(deadline) = deadline {
                if std::time::Instant::now() >= deadline {
                    return Err(ToolError::Rejected(
                        "request_user_input timed out waiting for a response".to_string(),
                    ));
                }
            }
            tokio::time::sleep(RESPONSE_POLL_INTERVAL).await;
        }
    }
}

/// Scan `events` for a response event at or after `from_index`, returning the
/// parsed answers it carries.
fn find_response(
    events: &[browser_use_protocol::EventRecord],
    from_index: usize,
) -> Option<RequestUserInputResponse> {
    events
        .iter()
        .skip(from_index)
        .find(|e| e.event_type == REQUEST_USER_INPUT_RESPONSE_DOTTED_EVENT)
        .map(|e| answers_from_payload(&e.payload))
}

/// Parse a response event payload into a [`RequestUserInputResponse`].
///
/// Accepts the codex wire shape `{ "answers": { id: { "answers": [..] } } }`
/// directly; tolerates a flat `{ "answers": { id: ["..", ".."] } }` shape (a
/// simpler TUI may write the latter) by coercing each value into an
/// [`UserInputAnswer`]. Unknown shapes yield an empty answer set rather than
/// erroring, so a malformed operator response never poisons the run.
fn answers_from_payload(payload: &serde_json::Value) -> RequestUserInputResponse {
    // Fast path: the exact codex wire shape deserializes directly.
    if let Ok(parsed) = serde_json::from_value::<RequestUserInputResponse>(payload.clone()) {
        return parsed;
    }
    let mut answers = HashMap::new();
    if let Some(map) = payload.get("answers").and_then(|v| v.as_object()) {
        for (id, value) in map {
            let list = value
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .or_else(|| value.as_str().map(|s| vec![s.to_string()]))
                .unwrap_or_default();
            answers.insert(id.clone(), UserInputAnswer { answers: list });
        }
    }
    RequestUserInputResponse { answers }
}

/// The async `request_user_input` tool.
///
/// Holds a pluggable [`RequestUserInputResponder`] (the host answer channel). The
/// default is the deterministic [`EchoAutoResponder`]; a real host injects its own
/// via [`with_responder`](RequestUserInputTool::with_responder). Cheap to clone
/// (the responder is shared behind an [`Arc`]). Performs no filesystem I/O.
#[derive(Clone)]
pub struct RequestUserInputTool {
    responder: Arc<dyn RequestUserInputResponder>,
}

impl std::fmt::Debug for RequestUserInputTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The responder is opaque (a trait object); show only the tool tag.
        f.debug_struct("RequestUserInputTool").finish()
    }
}

impl Default for RequestUserInputTool {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestUserInputTool {
    /// Construct a new `request_user_input` tool with the default
    /// [`EchoAutoResponder`] (deterministic auto-answer; host-free).
    pub fn new() -> Self {
        Self {
            responder: Arc::new(EchoAutoResponder),
        }
    }

    /// Construct the tool with a real host-backed [`RequestUserInputResponder`]
    /// (the production seam the dispatcher/entrypoint injects).
    pub fn with_responder(responder: Arc<dyn RequestUserInputResponder>) -> Self {
        Self { responder }
    }
}

/// Approval key: the question ids identify a call for session caching, mirroring
/// the shape the update_plan tool uses for its plan (`update_plan.rs:207-210`).
/// In practice this tool never prompts for exec approval (see below), so the key
/// is rarely consulted; it exists to satisfy the [`Approvable`] contract
/// uniformly.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct RequestUserInputApprovalKey {
    question_ids: Vec<String>,
}

impl Approvable<RequestUserInputRequest> for RequestUserInputTool {
    type ApprovalKey = RequestUserInputApprovalKey;

    fn approval_keys(&self, req: &RequestUserInputRequest) -> Vec<Self::ApprovalKey> {
        vec![RequestUserInputApprovalKey {
            question_ids: req.questions.iter().map(|q| q.id.clone()).collect(),
        }]
    }

    /// `request_user_input` touches no filesystem; request the default sandbox
    /// permissions (no escalation), mirroring the update_plan / shell tools
    /// (`update_plan.rs:236-238`, `shell.rs:242-244`).
    fn sandbox_permissions(&self, _req: &RequestUserInputRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` is intentionally left at its trait default
    // (`None`): codex's request_user_input handler needs no exec approval — it is
    // a plain `ToolKind::Function` handler with no approval gate
    // (`core/src/tools/handlers/request_user_input.rs`); the human interaction it
    // performs is the response round-trip itself, not an exec-policy prompt.
    // Returning `None` lets the orchestrator apply
    // `default_exec_approval_requirement`, which yields `Skip` under any
    // non-prompting policy. See the module doc.
}

impl Sandboxable for RequestUserInputTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide (today everything resolves to
        // `SandboxType::None`). Matches the update_plan / shell tools
        // (`update_plan.rs:249-255`, `shell.rs:261-267`). The tool does no I/O, so
        // the sandbox is moot, but `Auto` keeps the seam uniform.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // The tool never produces a sandbox denial (it does no I/O), so this is
        // moot; `true` keeps it uniform with the other tools
        // (`update_plan.rs:257-262`, `shell.rs:269-273`).
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<RequestUserInputRequest, ExecOutput> for RequestUserInputTool {
    fn parallel_safe(&self, _req: &RequestUserInputRequest) -> bool {
        // Match codex: SERIAL (false). Codex's request_user_input handler does
        // NOT override `supports_parallel_tool_calls`
        // (`core/src/tools/handlers/request_user_input.rs` has no such method), so
        // it inherits the trait default of `false`
        // (`codex-rs/tools/src/tool_executor.rs:51-53`, and
        // `core/src/tools/registry.rs:266-268` only ANDs in exposure). It is a
        // BLOCKING human interaction — it must run on the serial path so no other
        // tool reorders around the user's answer. We follow that exactly: `false`.
        false
    }

    async fn run(
        &self,
        req: &RequestUserInputRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox is exercised (the tool does no I/O); acknowledge the attempt
        // to make the seam explicit, matching the other tools.
        let _ = attempt;

        // Validate + normalize per codex's rules (non-empty options per question,
        // is_other forced true). A violation is a clean `Rejected`.
        let normalized = validate_request_user_input(req.clone())?;

        // HOST ROUND-TRIP. Codex calls `session.request_user_input(...).await` and
        // BLOCKS for the user's `RequestUserInputResponse`
        // (`core/src/tools/handlers/request_user_input.rs:65-87`); the legacy carve
        // appends a request event and blocks in
        // `wait_for_request_user_input_response`
        // (`browser-use-core/src/request_user_input.rs:141-163`). We model that with
        // the pluggable responder: surface the normalized questions to the host and
        // AWAIT the answers. The default `EchoAutoResponder` auto-answers (host-free
        // for tests); a real host injects its own responder.
        let response = self.responder.respond(&normalized).await?;

        // Return the ANSWERS to the model (codex/legacy behavior), prefixed so a
        // host-aware layer can recognize the completed round-trip.
        let payload = serde_json::to_string(&response).map_err(|err| {
            ToolError::Other(anyhow::anyhow!(
                "failed to serialize request_user_input response: {err}"
            ))
        })?;

        Ok(ExecOutput {
            exit_code: 0,
            stdout: format!("{REQUEST_USER_INPUT_STDOUT_PREFIX}{payload}"),
            stderr: String::new(),
        })
    }
}
