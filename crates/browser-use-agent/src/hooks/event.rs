//! Hook event kinds, the hook input payload, and the hook decision type.
//!
//! Parity sources:
//! - codex `core/src/hooks.rs` `HookEvent` enum + `HookDecision`
//!   (`/home/exedev/repos/codex/codex-rs/core/src/hooks.rs:7-62`).
//! - legacy `browser-use-core` `HookEventName`
//!   (`crates/browser-use-core/src/lib.rs:6812-6829`) — identical event-name
//!   set + PascalCase wire encoding.
//!
//! The codex/legacy set is `PreToolUse`, `PostToolUse`, `UserPromptSubmit`,
//! `Stop`, `SubagentStart`, `SubagentStop`, `Notification`, `SessionStart`.
//!
//! SANCTIONED ADDITIONS (explicitly requested by the user on top of codex's
//! set: "+PermissionRequest event + Prompt/Agent handler kinds"):
//! - [`HookEvent::PermissionRequest`] — fires when a tool call needs approval
//!   (see `tools/approval.rs` `ExecApprovalRequirement`). codex has no hook
//!   event for the approval/permission flow; this is the new kind that drives
//!   the `PermissionRequest` `PendingEvent`.
//! - [`HookEvent::Prompt`] — a prompt-lifecycle handler kind, firing around the
//!   prompt boundary (a generalization of codex's single `UserPromptSubmit`).
//! - [`HookEvent::Agent`] — an agent-lifecycle handler kind, firing around the
//!   agent/turn boundary (see `task/lifecycle.rs` `TurnLifecycleEvent`).
//!
//! These three are NOT present in codex `hooks.rs`; they are flagged here as
//! sanctioned additions and carry stable PascalCase wire names so config can
//! reference them just like the codex kinds.

use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

/// Identifies the lifecycle moment that triggered a hook invocation.
///
/// The first eight variants mirror codex `hooks.rs::HookEvent` exactly (and the
/// legacy `HookEventName`). The final three ([`HookEvent::PermissionRequest`],
/// [`HookEvent::Prompt`], [`HookEvent::Agent`]) are sanctioned additions
/// requested by the user — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HookEvent {
    // --- codex / legacy parity kinds ---
    /// Fired before a tool call is dispatched to its handler.
    PreToolUse,
    /// Fired after a tool call completes (success or failure).
    PostToolUse,
    /// Fired when the user submits a prompt to the agent.
    UserPromptSubmit,
    /// Fired when the agent finishes a turn and yields control.
    Stop,
    /// Fired when a subagent is spawned.
    SubagentStart,
    /// Fired when a subagent completes.
    SubagentStop,
    /// Fired when the agent emits a user-facing notification.
    Notification,
    /// Fired once when a session is created.
    SessionStart,

    // --- sanctioned additions (user-requested; not in codex hooks.rs) ---
    /// SANCTIONED ADDITION: fired when a tool call requires approval. Drives the
    /// `PermissionRequest` `PendingEvent` (see `runtime.rs`). Aligns with
    /// `tools/approval.rs` `ExecApprovalRequirement`.
    PermissionRequest,
    /// SANCTIONED ADDITION: a prompt-lifecycle handler kind (generalizes
    /// codex's single `UserPromptSubmit`).
    Prompt,
    /// SANCTIONED ADDITION: an agent-lifecycle handler kind, firing around the
    /// agent/turn boundary (see `task/lifecycle.rs` `TurnLifecycleEvent`).
    Agent,
}

impl HookEvent {
    /// The string name used in config keys + JSON payloads.
    ///
    /// Mirrors codex `HookEvent::as_str`
    /// (`/home/exedev/repos/codex/codex-rs/core/src/hooks.rs:28-39`); the three
    /// additions extend the same PascalCase scheme.
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::Notification => "Notification",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::Prompt => "Prompt",
            HookEvent::Agent => "Agent",
        }
    }
}

/// The payload passed to a hook command on stdin as JSON.
///
/// Mirrors codex `hook_runtime.rs::HookInput`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:23-46`):
/// `hook_event_name` is always present; the optional fields are populated per
/// event kind, and `extra` flattens any additional fields (e.g. the permission
/// reason for [`HookEvent::PermissionRequest`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookInput {
    /// The event name (e.g. "PreToolUse").
    pub hook_event_name: String,
    /// The session identifier, when available.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    /// The tool name, for tool events.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_name: Option<String>,
    /// Tool input arguments, for tool events.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_input: Option<JsonValue>,
    /// Tool output, for PostToolUse.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_response: Option<JsonValue>,
    /// The prompt text, for UserPromptSubmit / Prompt.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt: Option<String>,
    /// Arbitrary extra fields (flattened into the top-level JSON object).
    #[serde(flatten, default)]
    pub extra: HashMap<String, JsonValue>,
}

impl HookInput {
    /// Build a minimal input carrying only the event name.
    pub fn new(event: HookEvent) -> Self {
        Self {
            hook_event_name: event.as_str().to_string(),
            session_id: None,
            tool_name: None,
            tool_input: None,
            tool_response: None,
            prompt: None,
            extra: HashMap::new(),
        }
    }

    /// Attach a session id.
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    /// Attach a tool name.
    pub fn with_tool_name(mut self, tool_name: impl Into<String>) -> Self {
        self.tool_name = Some(tool_name.into());
        self
    }

    /// Attach tool input arguments.
    pub fn with_tool_input(mut self, tool_input: JsonValue) -> Self {
        self.tool_input = Some(tool_input);
        self
    }

    /// Attach a tool response (PostToolUse).
    pub fn with_tool_response(mut self, tool_response: JsonValue) -> Self {
        self.tool_response = Some(tool_response);
        self
    }

    /// Attach a prompt string.
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }

    /// Insert an arbitrary extra field.
    pub fn with_extra(mut self, key: impl Into<String>, value: JsonValue) -> Self {
        self.extra.insert(key.into(), value);
        self
    }
}

/// Decision returned by a hook command, parsed from its stdout JSON.
///
/// Mirrors codex `hooks.rs::HookDecision`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hooks.rs:43-62`):
/// `continue: Some(false)` blocks the action; `reason` is a human-readable
/// explanation; `additional_context` is injected back into the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HookDecision {
    /// When `Some(false)`, the action is denied/blocked.
    #[serde(default)]
    pub r#continue: Option<bool>,
    /// Optional human-readable reason shown to the user/agent.
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional additional context to inject back into the model.
    #[serde(default)]
    pub additional_context: Option<String>,
}

impl HookDecision {
    /// True when this decision blocks the action.
    ///
    /// Matches codex `HookDecision::is_block`
    /// (`/home/exedev/repos/codex/codex-rs/core/src/hooks.rs:59-61`).
    pub fn is_block(&self) -> bool {
        self.r#continue == Some(false)
    }
}
