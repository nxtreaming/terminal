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
//! # Request side only — host round-trip is DEFERRED (TODO)
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
//! The new `browser-use-agent` crate does NOT yet have that UI/host wiring, so
//! this WP models the **request side only**: it
//! [validates](validate_request_user_input) the args (faithful to codex's
//! `normalize_request_user_input_args`,
//! `core/src/tools/handlers/request_user_input_spec.rs:99-115`), normalizes them
//! (forcing `is_other = true` on every question, exactly as codex/legacy do), and
//! emits the structured "user input requested" payload as JSON into
//! [`ExecOutput::stdout`] (prefixed with [`REQUEST_USER_INPUT_STDOUT_PREFIX`] so a
//! later host-aware layer can recognize it). It does NOT read stdin and does NOT
//! block waiting for a human.
//!
//! TODO(WP-T-request_user_input-host-roundtrip): wire `run` to a real
//! session/host channel that surfaces the request (the
//! [`REQUEST_USER_INPUT_REQUEST_EVENT`] event), blocks for the user's
//! [`RequestUserInputResponse`] (the [`REQUEST_USER_INPUT_RESPONSE_EVENT`] event),
//! and returns the serialized response as codex/legacy do
//! (`core/src/tools/handlers/request_user_input.rs:65-87`;
//! `browser-use-core/src/request_user_input.rs:141-163`). The codex root-thread
//! gate ("request_user_input can only be used by the root thread",
//! `request_user_input.rs:54-58`) and the mode-availability gate
//! (`request_user_input_unavailable_message`) are likewise session-layer concerns
//! deferred here. Until then `run` returns the request representation, not the
//! answer.
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

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

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

/// The async `request_user_input` tool.
///
/// Stateless; cheap to clone/construct. Performs no I/O and reads no stdin (see
/// the module doc: this WP implements the request side only; the host round-trip
/// is deferred).
#[derive(Clone, Debug, Default)]
pub struct RequestUserInputTool;

impl RequestUserInputTool {
    /// Construct a new `request_user_input` tool.
    pub fn new() -> Self {
        Self
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

        // HOST ROUND-TRIP DEFERRED (see the module doc). Codex would now call
        // `session.request_user_input(...).await` and BLOCK for the user's
        // `RequestUserInputResponse`
        // (`core/src/tools/handlers/request_user_input.rs:65-87`); the legacy carve
        // appends a request event and blocks in
        // `wait_for_request_user_input_response`
        // (`browser-use-core/src/request_user_input.rs:141-163`). The new crate has
        // no UI/host wiring yet, so we emit the structured REQUEST payload (NOT the
        // answer) into stdout, prefixed so a later host-aware layer can recognize
        // it and complete the round-trip. We do NOT read stdin / block.
        let payload = serde_json::to_string(&normalized).map_err(|err| {
            ToolError::Other(anyhow::anyhow!(
                "failed to serialize request_user_input request: {err}"
            ))
        })?;

        Ok(ExecOutput {
            exit_code: 0,
            stdout: format!("{REQUEST_USER_INPUT_STDOUT_PREFIX}{payload}"),
            stderr: String::new(),
        })
    }
}
