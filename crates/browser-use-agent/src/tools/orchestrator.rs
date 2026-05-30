//! `ToolOrchestrator` — async driver over the pure `plan_attempts` decision table.

use super::approval::AskForApproval;
use super::runtime::{ToolCtx, ToolError, ToolRuntime};
use super::sandbox::{FileSystemSandboxPolicy, NoneSandboxProvider, SandboxProvider};
use super::Approver;
use super::AutoApprover;

pub struct TurnEnv {
    pub file_system_sandbox_policy: FileSystemSandboxPolicy,
    pub managed_network_active: bool,
    pub strict_auto_review: bool,
    pub use_guardian: bool,
}

pub struct OrchestratorRunResult<Out> {
    pub output: Out,
}

pub struct ToolOrchestrator<S: SandboxProvider, A: Approver> {
    // sandbox, approver, approvals: tokio::Mutex<ApprovalStore>
    _s: std::marker::PhantomData<(S, A)>,
}

impl ToolOrchestrator<NoneSandboxProvider, AutoApprover> {
    /// Sandbox = None + auto-approve path.
    pub fn stub() -> Self {
        unimplemented!()
    }
}

impl<S: SandboxProvider, A: Approver> ToolOrchestrator<S, A> {
    pub async fn run<Req, Out, T>(
        &self,
        _tool: &T,
        _req: &Req,
        _ctx: &ToolCtx,
        _env: &TurnEnv,
        _policy: AskForApproval,
    ) -> Result<OrchestratorRunResult<Out>, ToolError>
    where
        T: ToolRuntime<Req, Out>,
        Req: Send + Sync,
        Out: Send,
    {
        unimplemented!()
    }
}
