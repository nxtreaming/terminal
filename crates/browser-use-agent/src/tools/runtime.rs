//! The trait stack a tool implements ONCE, plus the PURE tool-decision functions.
//!
//! The pure fns (`default_exec_approval_requirement`, `sandbox_override_for_first_attempt`,
//! `plan_attempts`, `map_decision`, `build_denial_reason`) live here rather than in
//! `decision/` because they reference the sandbox/approval enums in this module.

use std::path::PathBuf;

use super::approval::{AskForApproval, ExecApprovalRequirement, ReviewDecision};
use super::sandbox::{
    FileSystemSandboxPolicy, SandboxLaunch, SandboxOverride, SandboxPermissions, SandboxPreference,
    SandboxType,
};

pub struct ToolCtx {
    pub call_id: String,
    pub tool_name: String,
    pub cwd: PathBuf,
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

    /// `sandboxing.rs:305`.
    fn should_bypass_approval(&self, policy: AskForApproval, already_approved: bool) -> bool {
        already_approved || matches!(policy, AskForApproval::Never)
    }

    /// `sandboxing.rs:320`.
    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        matches!(
            policy,
            AskForApproval::OnFailure | AskForApproval::UnlessTrusted
        )
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

pub fn default_exec_approval_requirement(
    _policy: AskForApproval,
    _fs: &FileSystemSandboxPolicy,
) -> ExecApprovalRequirement {
    unimplemented!()
}

pub fn sandbox_override_for_first_attempt(
    _perms: SandboxPermissions,
    _req: &ExecApprovalRequirement,
    _fs: &FileSystemSandboxPolicy,
) -> SandboxOverride {
    unimplemented!()
}

pub fn map_decision(
    _d: ReviewDecision,
    _guardian_review_id: Option<&str>,
) -> Result<(), ToolError> {
    unimplemented!()
}

pub fn build_denial_reason(_host: Option<&str>) -> String {
    unimplemented!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenialAction {
    Return,
    RetryNone { needs_reapproval: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptPlan {
    pub needs_initial_approval: bool,
    pub initial_sandbox: SandboxType,
    pub on_denial: DenialAction,
}

/// PURE state machine factoring `orchestrator.rs:145-388` into a decision table.
#[allow(clippy::too_many_arguments)]
pub fn plan_attempts(
    _requirement: &ExecApprovalRequirement,
    _ovr: SandboxOverride,
    _escalate_on_failure: bool,
    _wants_no_sandbox: bool,
    _should_bypass: bool,
    _strict_auto_review: bool,
    _already_approved: bool,
    _net_denial: bool,
) -> AttemptPlan {
    unimplemented!()
}
