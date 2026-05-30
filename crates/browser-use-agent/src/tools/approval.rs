//! Approval policy + decision types (codex `sandboxing.rs` parity).
//!
//! Pure value types: how aggressively to ask the user before running a tool
//! ([`AskForApproval`]), the user's verdict ([`ReviewDecision`]), the resolved
//! requirement for a single call ([`ExecApprovalRequirement`]), and the
//! session approval cache ([`ApprovalStore`]). The orchestration *flow* that
//! consumes these lives in [`crate::tools::runtime`] (the pure decision fns) and
//! the Wave-2 `orchestrator`.

/// How aggressively the agent asks the user before running a tool.
///
/// Codex parity: `AskForApproval` (codex_protocol::protocol). `Granular` carries
/// per-feature toggles ([`GranularApprovalConfig`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskForApproval {
    /// Never prompt; fully non-interactive.
    Never,
    /// Run sandboxed; only ask if the sandbox rejects the command.
    OnFailure,
    /// Ask unless the filesystem access is unrestricted.
    OnRequest,
    /// Always ask before running.
    UnlessTrusted,
    /// Per-feature approval toggles.
    Granular(GranularApprovalConfig),
}

/// Per-feature approval toggles for [`AskForApproval::Granular`].
///
/// Codex parity: `GranularApprovalConfig`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GranularApprovalConfig {
    /// Whether the user may be prompted to retry a command without the sandbox.
    pub sandbox_approval: bool,
}

impl GranularApprovalConfig {
    /// Whether granular config permits a no-sandbox approval prompt.
    pub fn allows_sandbox_approval(&self) -> bool {
        self.sandbox_approval
    }
}

/// The user's verdict on an approval request.
///
/// Codex parity: `ReviewDecision`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// Run once.
    Approved,
    /// Run and remember the approval for the rest of the session.
    ApprovedForSession,
    /// Do not run.
    Denied,
    /// Stop the turn entirely.
    Abort,
    /// The approval request expired without an answer.
    TimedOut,
}

impl ReviewDecision {
    /// `true` for [`ReviewDecision::Approved`] / [`ReviewDecision::ApprovedForSession`].
    ///
    /// Codex parity: `ReviewDecision::is_approved`.
    pub fn is_approved(self) -> bool {
        matches!(self, Self::Approved | Self::ApprovedForSession)
    }
}

/// What the orchestrator should do with a given tool call's approval gate.
///
/// Codex parity: `ExecApprovalRequirement` (sandboxing.rs:158-179). The codex
/// `proposed_execpolicy_amendment` fields are part of the exec-policy subsystem
/// (not in this WP's scope) and are intentionally elided from the frozen shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecApprovalRequirement {
    /// No approval required.
    Skip {
        /// The first attempt should skip sandboxing (explicitly greenlit).
        bypass_sandbox: bool,
    },
    /// Approval required before running.
    NeedsApproval { reason: Option<String> },
    /// Execution forbidden outright.
    Forbidden { reason: String },
}

/// Session-scoped record of which tool calls the user has already approved.
///
/// Codex parity: `ApprovalStore` (sandboxing.rs:39-62). Keys are serialized with
/// serde so any `Hash + Eq + Serialize` approval key (e.g. a per-file path for
/// `apply_patch`, a command digest for `shell`) shares one cache.
#[derive(Clone, Default, Debug)]
pub struct ApprovalStore {
    /// Store serialized keys for generic caching across requests.
    map: std::collections::HashMap<String, ReviewDecision>,
}

impl ApprovalStore {
    /// Look up the cached decision for `key`, if any.
    ///
    /// Codex parity: `ApprovalStore::get` (sandboxing.rs:46-52). Keys that fail
    /// to serialize never matched a `put`, so they return `None`.
    pub fn get<K: serde::Serialize>(&self, key: &K) -> Option<ReviewDecision> {
        let s = serde_json::to_string(key).ok()?;
        self.map.get(&s).copied()
    }

    /// Record `value` for `key`.
    ///
    /// Codex parity: `ApprovalStore::put` (sandboxing.rs:54-61).
    pub fn put<K: serde::Serialize>(&mut self, key: K, value: ReviewDecision) {
        if let Ok(s) = serde_json::to_string(&key) {
            self.map.insert(s, value);
        }
    }
}
