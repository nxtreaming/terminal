//! The trait stack a tool implements ONCE, plus the PURE tool-decision functions.
//!
//! A tool implements [`ToolRuntime`] (= [`Approvable`] + [`Sandboxable`] + an
//! async `run`); the orchestrator (Wave-2) drives it through the
//! approval → select → run → escalate → retry-`None` flow using the default
//! methods here plus the pure fns at the bottom of this file.
//!
//! The pure fns (`default_exec_approval_requirement`, `sandbox_override_for_first_attempt`,
//! `plan_attempts`, `map_decision`, `build_denial_reason`) live here rather than in
//! `decision/` because they reference the sandbox/approval enums in this module;
//! they are nonetheless PURE (no I/O, no async) and exhaustively unit-tested.
//!
//! Ground truth: codex `core/src/tools/sandboxing.rs` (the trait stack +
//! `default_exec_approval_requirement` / `sandbox_override_for_first_attempt`)
//! and `core/src/tools/orchestrator.rs:145-388` (the flow `plan_attempts`
//! distills into a pure decision table).

use std::path::PathBuf;

use super::approval::{AskForApproval, ExecApprovalRequirement, ReviewDecision};
use super::sandbox::{
    FileSystemSandboxPolicy, SandboxLaunch, SandboxOverride, SandboxPermissions, SandboxPreference,
    SandboxType,
};

/// Context handed to a tool when it runs. Codex parity: `ToolCtx`.
#[derive(Clone)]
pub struct ToolCtx {
    pub call_id: String,
    pub tool_name: String,
    pub cwd: PathBuf,
    pub artifact_root: PathBuf,
}

pub struct SandboxAttempt<'a> {
    pub sandbox: SandboxType,
    pub permissions: SandboxPermissions,
    pub enforce_managed_network: bool,
    pub launch: &'a SandboxLaunch,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}

#[derive(Debug)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub struct NetworkPolicyDecision {
    pub host: String,
}

#[derive(Debug)]
pub struct SandboxDenial {
    pub output: ExecOutput,
    pub network_policy_decision: Option<NetworkPolicyDecision>,
}

#[derive(Debug)]
pub enum ToolError {
    Rejected(String),
    Sandboxed(SandboxDenial),
    Other(anyhow::Error),
}

pub struct ApprovalRequest<'a> {
    pub ctx: &'a ToolCtx,
    pub reason: Option<String>,
    pub guardian_review_id: Option<String>,
}

pub struct PermissionRequestPayload(pub serde_json::Value);

pub trait Approvable<Req> {
    type ApprovalKey: std::hash::Hash + Eq + Clone + std::fmt::Debug + serde::Serialize;

    fn approval_keys(&self, req: &Req) -> Vec<Self::ApprovalKey>;

    fn sandbox_permissions(&self, _req: &Req) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    fn exec_approval_requirement(&self, _req: &Req) -> Option<ExecApprovalRequirement> {
        None
    }

    /// May we skip prompting for this call? Default delegates to the codex
    /// parity logic in [`should_bypass_approval`] (`sandboxing.rs:305`).
    fn should_bypass_approval(&self, policy: AskForApproval, already_approved: bool) -> bool {
        should_bypass_approval(policy, already_approved)
    }

    /// After a sandbox denial, may we ask to retry unsandboxed? Default
    /// delegates to [`wants_no_sandbox_approval`] (`sandboxing.rs:320`).
    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        wants_no_sandbox_approval(policy)
    }

    fn permission_request_payload(&self, _req: &Req) -> Option<PermissionRequestPayload> {
        None
    }
}

pub trait Sandboxable {
    fn sandbox_preference(&self) -> SandboxPreference;

    fn escalate_on_failure(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
pub trait ToolRuntime<Req: Send + Sync, Out: Send>:
    Approvable<Req> + Sandboxable + Send + Sync
{
    /// `-> decision::classify_parallelism`.
    fn parallel_safe(&self, _req: &Req) -> bool {
        false
    }

    async fn run(
        &self,
        req: &Req,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<Out, ToolError>;
}

#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    async fn review(&self, payload: ApprovalRequest<'_>) -> ReviewDecision;
}

/// Stub auto-approver.
pub struct AutoApprover;

#[async_trait::async_trait]
impl Approver for AutoApprover {
    async fn review(&self, _p: ApprovalRequest<'_>) -> ReviewDecision {
        ReviewDecision::Approved
    }
}

// ---- PURE decision fns (sandboxing.rs:202-334, orchestrator.rs:145-388) ----

/// Should the approval prompt be skipped entirely?
///
/// Codex parity: `Approvable::should_bypass_approval` (sandboxing.rs:305-311):
/// an already-cached approval bypasses; otherwise only `Never` is silent.
pub fn should_bypass_approval(policy: AskForApproval, already_approved: bool) -> bool {
    if already_approved {
        // We do not ask one more time.
        return true;
    }
    matches!(policy, AskForApproval::Never)
}

/// After a sandbox denial, may we ask the user to retry *unsandboxed*?
///
/// Codex parity: `Approvable::wants_no_sandbox_approval` (sandboxing.rs:326-334).
pub fn wants_no_sandbox_approval(policy: AskForApproval) -> bool {
    match policy {
        AskForApproval::OnFailure => true,
        AskForApproval::UnlessTrusted => true,
        AskForApproval::Never => false,
        AskForApproval::OnRequest => false,
        AskForApproval::Granular(granular_config) => granular_config.sandbox_approval,
    }
}

/// Default approval requirement for an exec-style call, keyed on policy.
///
/// Codex parity: `default_exec_approval_requirement` (sandboxing.rs:202-238):
/// - `Never` / `OnFailure`: do not ask (`Skip`).
/// - `OnRequest` / `Granular`: ask iff the filesystem is restricted.
/// - `Granular` with `sandbox_approval` disabled and approval needed:
///   `Forbidden`.
/// - `UnlessTrusted`: always ask (`NeedsApproval`).
pub fn default_exec_approval_requirement(
    policy: AskForApproval,
    fs: &FileSystemSandboxPolicy,
) -> ExecApprovalRequirement {
    let needs_approval = match policy {
        AskForApproval::Never | AskForApproval::OnFailure => false,
        AskForApproval::OnRequest | AskForApproval::Granular(_) => fs.is_restricted(),
        AskForApproval::UnlessTrusted => true,
    };

    if needs_approval
        && matches!(
            policy,
            AskForApproval::Granular(granular_config)
                if !granular_config.allows_sandbox_approval()
        )
    {
        ExecApprovalRequirement::Forbidden {
            reason: "approval policy disallowed sandbox approval prompt".to_string(),
        }
    } else if needs_approval {
        ExecApprovalRequirement::NeedsApproval { reason: None }
    } else {
        ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
        }
    }
}

/// Compute the sandbox override for the first attempt.
///
/// Codex parity: `sandbox_override_for_first_attempt` (sandboxing.rs:246-274):
/// 1. `Skip { bypass_sandbox: true }` (full trust) bypasses, overriding any
///    escalated-permission hint.
/// 2. Deny-read restrictions suppress escalation (the override would discard the
///    filesystem policy).
/// 3. Otherwise, escalated permissions bypass the sandbox for the first attempt.
pub fn sandbox_override_for_first_attempt(
    perms: SandboxPermissions,
    req: &ExecApprovalRequirement,
    fs: &FileSystemSandboxPolicy,
) -> SandboxOverride {
    if matches!(
        req,
        ExecApprovalRequirement::Skip {
            bypass_sandbox: true,
        }
    ) {
        return SandboxOverride::BypassSandboxFirstAttempt;
    }

    if fs.has_denied_read_restrictions() {
        return SandboxOverride::NoOverride;
    }

    if perms.requires_escalated_permissions() {
        SandboxOverride::BypassSandboxFirstAttempt
    } else {
        SandboxOverride::NoOverride
    }
}

/// Map a review decision into a run/deny outcome.
///
/// Codex parity: the orchestrator's `reject_if_not_approved` (orchestrator.rs):
/// approving decisions are `Ok`; everything else (`Denied`/`Abort`/`TimedOut`)
/// is a [`ToolError::Rejected`]. `guardian_review_id`, when present, is woven
/// into the rejection message so denial handling can refer to the review.
pub fn map_decision(d: ReviewDecision, guardian_review_id: Option<&str>) -> Result<(), ToolError> {
    if d.is_approved() {
        return Ok(());
    }
    let what = match d {
        ReviewDecision::Denied => "rejected by user",
        ReviewDecision::Abort => "aborted by user",
        ReviewDecision::TimedOut => "approval timed out",
        ReviewDecision::Approved | ReviewDecision::ApprovedForSession => unreachable!(),
    };
    let msg = match guardian_review_id {
        Some(id) => format!("command {what} (review {id})"),
        None => format!("command {what}"),
    };
    Err(ToolError::Rejected(msg))
}

/// Build the human-readable retry reason for a sandbox denial.
///
/// Codex parity: the orchestrator's `build_denial_reason_from_output` /
/// network-denial branch (orchestrator.rs:188-210). A blocked host is called out
/// explicitly; otherwise a generic sandbox-failure message is used.
pub fn build_denial_reason(host: Option<&str>) -> String {
    match host {
        Some(host) => format!("Network access to \"{host}\" is blocked by policy."),
        None => "Command failed while running in the sandbox.".to_string(),
    }
}

/// What to do after a sandbox denial on the first attempt.
///
/// Codex parity: the escalation arm of `ToolOrchestrator::run`
/// (orchestrator.rs:188-265).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenialAction {
    /// Surface the denial; do not retry.
    Return,
    /// Retry the attempt with `SandboxType::None`.
    RetryNone {
        /// Whether a fresh approval is required before the unsandboxed retry.
        needs_reapproval: bool,
    },
}

/// The plan for a tool call: the approval gate, the first-attempt sandbox, and
/// what to do if that attempt is denied.
///
/// A pure factoring of `ToolOrchestrator::run` (orchestrator.rs:145-388).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptPlan {
    /// Whether an approval must be obtained before the first attempt.
    pub needs_initial_approval: bool,
    /// The sandbox for the first attempt.
    pub initial_sandbox: SandboxType,
    /// What to do if the first attempt is denied by the sandbox.
    pub on_denial: DenialAction,
}

/// PURE state machine factoring `orchestrator.rs:145-388` into a decision table.
///
/// Inputs (all pre-computed by callers from the tool + policy):
/// - `requirement`: the resolved [`ExecApprovalRequirement`].
/// - `ovr`: the first-attempt [`SandboxOverride`] from
///   [`sandbox_override_for_first_attempt`].
/// - `escalate_on_failure`: the tool's [`Sandboxable::escalate_on_failure`].
/// - `wants_no_sandbox`: the tool's [`Approvable::wants_no_sandbox_approval`]
///   for the active policy.
/// - `should_bypass`: [`should_bypass_approval`] for the active policy.
/// - `strict_auto_review`: whether guardian strict-auto-review is on.
/// - `already_approved`: whether the call is already approved this session.
/// - `net_denial`: whether the (modeled) denial is a network-policy denial,
///   which always forces a fresh approval on the unsandboxed retry.
///
/// Decision:
/// - `needs_initial_approval`: `NeedsApproval`/`Forbidden` always; `Skip` only
///   under `strict_auto_review`.
/// - `initial_sandbox`: `None` when the override bypasses the sandbox, else
///   `Restricted` (the configured sandbox under `NoOverride`).
/// - `on_denial`: `Return` if the tool does not escalate on failure, or if the
///   policy does not want a no-sandbox approval; otherwise `RetryNone`, with a
///   fresh approval required unless approval is bypassable *and* this is not a
///   network denial (which always re-prompts), and never bypassed under
///   strict-auto-review.
#[allow(clippy::too_many_arguments)]
pub fn plan_attempts(
    requirement: &ExecApprovalRequirement,
    ovr: SandboxOverride,
    escalate_on_failure: bool,
    wants_no_sandbox: bool,
    should_bypass: bool,
    strict_auto_review: bool,
    already_approved: bool,
    net_denial: bool,
) -> AttemptPlan {
    let _ = already_approved; // folded into `should_bypass` by the caller.

    let needs_initial_approval = match requirement {
        ExecApprovalRequirement::Skip { .. } => strict_auto_review,
        ExecApprovalRequirement::NeedsApproval { .. }
        | ExecApprovalRequirement::Forbidden { .. } => true,
    };

    let initial_sandbox = match ovr {
        SandboxOverride::BypassSandboxFirstAttempt => SandboxType::None,
        SandboxOverride::NoOverride => SandboxType::Restricted,
    };

    let on_denial = if !escalate_on_failure || !wants_no_sandbox {
        DenialAction::Return
    } else {
        // Strict auto-review approval only covers the sandboxed attempt; the
        // unsandboxed retry needs a fresh review. A network denial likewise
        // always re-prompts. Otherwise a bypassable approval skips the prompt.
        let bypass_retry_approval = !strict_auto_review && should_bypass && !net_denial;
        DenialAction::RetryNone {
            needs_reapproval: !bypass_retry_approval,
        }
    };

    AttemptPlan {
        needs_initial_approval,
        initial_sandbox,
        on_denial,
    }
}
