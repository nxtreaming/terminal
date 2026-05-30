//! LLM-reviewer seam for the guardian safety gate.
//!
//! browser-use addition (NOT codex parity): the guardian runs an
//! LLM-reviewer as a safety gate on each gated tool call. This file only
//! defines the *seam* — an async trait whose production implementation
//! drives a model, while tests inject a fake reviewer so the test suite is
//! network-free.
//!
//! The closest codex analog is the review task in
//! `codex-rs/core/src/tasks/review.rs` (`run_review_task`, review.rs:20),
//! which drives a model turn and parses a structured verdict. We mirror the
//! "drive a model, parse a structured verdict" shape but keep it behind an
//! injectable trait so the guardian can be unit-tested without a network.

use async_trait::async_trait;

/// Request handed to the LLM-reviewer describing the tool/command + context.
///
/// browser-use addition: carries the minimum the reviewer needs to judge a
/// single gated tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardianRequest {
    /// Name of the tool being reviewed.
    pub tool_name: String,
    /// Raw arguments / command payload under review.
    pub arguments: String,
    /// Free-form context the caller wants the reviewer to consider.
    pub context: String,
}

impl GuardianRequest {
    /// Build a review request for the given tool + arguments.
    pub fn new(tool_name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            arguments: arguments.into(),
            context: String::new(),
        }
    }

    /// Attach additional context to the request.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = context.into();
        self
    }

    /// Stable cache key for this request (tool + arguments).
    ///
    /// Mirrors codex `SessionApprovalCache`'s command-keyed cache
    /// (`codex-rs/core/src/tools/sandboxing.rs:18`), which keys on the
    /// command vector; we key on `(tool_name, arguments)`.
    pub fn cache_key(&self) -> String {
        format!("{}\u{0}{}", self.tool_name, self.arguments)
    }
}

/// Verdict returned by the LLM-reviewer.
///
/// browser-use addition. Maps onto codex `ReviewDecision`
/// (`codex-rs/protocol/src/protocol.rs:1389`): `Allow` ~ `Approved`,
/// `Deny` ~ `Denied`, `Escalate` ~ `Prompt`/human-in-the-loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardianVerdict {
    /// Reviewer judged the call safe.
    Allow,
    /// Reviewer judged the call unsafe; deny with a reason.
    Deny { reason: String },
    /// Reviewer is uncertain; escalate to a human decision.
    Escalate { reason: String },
}

/// Error produced when the reviewer cannot deliver a verdict.
///
/// FAIL-CLOSED contract: the [`Guardian`](crate::guardian::Guardian) treats
/// *any* `ReviewerError` (including timeout) as a deny, never an allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewerError {
    /// The reviewer timed out before producing a verdict.
    Timeout,
    /// The reviewer failed for some other reason.
    Failed { message: String },
}

impl std::fmt::Display for ReviewerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewerError::Timeout => write!(f, "guardian reviewer timed out"),
            ReviewerError::Failed { message } => {
                write!(f, "guardian reviewer failed: {message}")
            }
        }
    }
}

impl std::error::Error for ReviewerError {}

/// The injectable LLM-reviewer seam.
///
/// browser-use addition. Production impls drive a model (analogous to
/// codex `run_review_task`, review.rs:20); tests inject a fake reviewer so
/// the suite stays network-free.
#[async_trait]
pub trait GuardianReviewer: Send + Sync + 'static {
    /// Review a single gated tool invocation and return a verdict.
    ///
    /// Returning `Err` is treated as fail-closed (deny) by the guardian.
    async fn review(&self, req: &GuardianRequest) -> Result<GuardianVerdict, ReviewerError>;
}
