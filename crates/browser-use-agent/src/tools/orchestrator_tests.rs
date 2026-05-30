//! Async tests for [`ToolOrchestrator::run`] (WP-B1).
//!
//! These exercise the real delegation path: the orchestrator drives the merged
//! pure helpers ([`plan_attempts`](super::runtime::plan_attempts),
//! [`map_decision`](super::runtime::map_decision),
//! [`default_exec_approval_requirement`](super::runtime::default_exec_approval_requirement),
//! ...) through tiny in-test [`ToolRuntime`]/[`Approver`] impls. No network, no
//! real sandbox.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::approval::{AskForApproval, ReviewDecision};
use super::orchestrator::{ToolOrchestrator, TurnEnv};
use super::runtime::{
    Approvable, ApprovalRequest, Approver, AutoApprover, ExecOutput, SandboxAttempt, SandboxDenial,
    Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use super::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions,
    SandboxPreference, SandboxProvider, SandboxType,
};

// ---- Test fixtures ---------------------------------------------------------

/// A trivial request; identity is keyed solely off its `id`.
#[derive(Clone)]
struct TestReq {
    id: &'static str,
}

/// A tool that always succeeds, recording the sandbox each attempt ran under.
struct EchoTool {
    seen: Arc<std::sync::Mutex<Vec<SandboxType>>>,
    runs: Arc<AtomicUsize>,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            runs: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Approvable<TestReq> for EchoTool {
    type ApprovalKey = String;

    fn approval_keys(&self, req: &TestReq) -> Vec<Self::ApprovalKey> {
        vec![format!("echo:{}", req.id)]
    }
}

impl Sandboxable for EchoTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Auto
    }
}

#[async_trait::async_trait]
impl ToolRuntime<TestReq, String> for EchoTool {
    async fn run(
        &self,
        _req: &TestReq,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<String, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(attempt.sandbox);
        Ok(format!("ran {} under {:?}", ctx.call_id, attempt.sandbox))
    }
}

/// A tool denied by the sandbox on its first (sandboxed) attempt, but that
/// succeeds when run under [`SandboxType::None`]. Records every attempt.
struct DenyingTool {
    seen: Arc<std::sync::Mutex<Vec<SandboxType>>>,
    /// When true, *every* attempt is denied (used to test `Return`).
    always_deny: bool,
}

impl DenyingTool {
    fn new(always_deny: bool) -> Self {
        Self {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            always_deny,
        }
    }
}

impl Approvable<TestReq> for DenyingTool {
    type ApprovalKey = String;

    fn approval_keys(&self, req: &TestReq) -> Vec<Self::ApprovalKey> {
        vec![format!("deny:{}", req.id)]
    }
}

impl Sandboxable for DenyingTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Auto
    }
    // Default `escalate_on_failure() == true` keeps the retry path live.
}

#[async_trait::async_trait]
impl ToolRuntime<TestReq, String> for DenyingTool {
    async fn run(
        &self,
        _req: &TestReq,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<String, ToolError> {
        self.seen.lock().unwrap().push(attempt.sandbox);
        if self.always_deny || attempt.sandbox != SandboxType::None {
            Err(ToolError::Sandboxed(SandboxDenial {
                output: ExecOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "denied by test sandbox".to_string(),
                },
                network_policy_decision: None,
            }))
        } else {
            Ok(format!("ran under {:?}", attempt.sandbox))
        }
    }
}

/// An approver returning a scripted decision, counting invocations.
struct ScriptedApprover {
    decision: ReviewDecision,
    calls: Arc<AtomicUsize>,
}

impl ScriptedApprover {
    fn new(decision: ReviewDecision) -> Self {
        Self {
            decision,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl Approver for ScriptedApprover {
    async fn review(&self, _payload: ApprovalRequest<'_>) -> ReviewDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.decision
    }
}

/// A provider that forces the first-attempt sandbox to a fixed flavor, so the
/// `DenyingTool` starts sandboxed and we can drive the denial path even though
/// `NoneSandboxProvider` would otherwise pick `None`.
struct FixedSandboxProvider(SandboxType);

impl SandboxProvider for FixedSandboxProvider {
    fn select_initial(
        &self,
        _fs: &FileSystemSandboxPolicy,
        _pref: SandboxPreference,
        _managed_network: bool,
    ) -> SandboxType {
        self.0
    }

    fn prepare(
        &self,
        sandbox: SandboxType,
        _cwd: &std::path::Path,
        _perms: SandboxPermissions,
    ) -> SandboxLaunch {
        SandboxLaunch {
            sandbox,
            cancel: None,
        }
    }
}

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "call-1".to_string(),
        tool_name: "test-tool".to_string(),
        cwd: std::path::PathBuf::from("/tmp/wp-b1"),
    }
}

/// A turn env with a given filesystem restriction.
fn env(restricted: bool) -> TurnEnv {
    TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    }
}

const REQ: TestReq = TestReq { id: "r1" };

// ---- Tests -----------------------------------------------------------------

/// (1) The `stub()` (NoneSandboxProvider + AutoApprover) runs a tool and
/// returns its output, under `SandboxType::None`.
#[tokio::test]
async fn stub_runs_tool_and_returns_output() {
    let orch = ToolOrchestrator::stub();
    let tool = EchoTool::new();
    let result = orch
        .run(&tool, &REQ, &ctx(), &env(false), AskForApproval::Never)
        .await
        .expect("stub run should succeed");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert!(result.output.contains("call-1"));
    assert_eq!(tool.runs.load(Ordering::SeqCst), 1);
    // `Never` => no approval prompt, nothing cached for session.
    assert!(!result.approved_for_session);
}

/// (2) An approval-required tool consults the approver and caches
/// `ApprovedForSession`; a second call with the same key does not re-prompt.
#[tokio::test]
async fn approval_required_caches_for_session() {
    let approver = ScriptedApprover::new(ReviewDecision::ApprovedForSession);
    let calls = approver.calls.clone();
    let orch = ToolOrchestrator::new(NoneSandboxProvider, approver);
    let tool = EchoTool::new();
    // `UnlessTrusted` => default requirement is `NeedsApproval` (always ask).
    let policy = AskForApproval::UnlessTrusted;

    let first = orch
        .run(&tool, &REQ, &ctx(), &env(false), policy)
        .await
        .expect("first run approved");
    assert!(first.approved_for_session);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "first call prompts");

    let second = orch
        .run(&tool, &REQ, &ctx(), &env(false), policy)
        .await
        .expect("second run uses cache");
    assert!(second.approved_for_session);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second call must not re-prompt (cache hit)"
    );
    assert_eq!(tool.runs.load(Ordering::SeqCst), 2);
}

/// (3) A `Denied` review yields a `ToolError::Rejected` and the tool never runs.
#[tokio::test]
async fn denied_review_returns_error_without_running() {
    let approver = ScriptedApprover::new(ReviewDecision::Denied);
    let orch = ToolOrchestrator::new(NoneSandboxProvider, approver);
    let tool = EchoTool::new();

    let err = orch
        .run(
            &tool,
            &REQ,
            &ctx(),
            &env(false),
            AskForApproval::UnlessTrusted,
        )
        .await
        .expect_err("denied review must error");

    assert!(matches!(err, ToolError::Rejected(_)));
    assert_eq!(
        tool.runs.load(Ordering::SeqCst),
        0,
        "tool must not run when denied"
    );
}

/// (4) A sandbox denial with `on_denial = RetryNone` re-runs under
/// `SandboxType::None` and succeeds.
///
/// `OnFailure` => `default_exec_approval_requirement` is `Skip` (no initial
/// approval) and `wants_no_sandbox_approval` is true, so `plan_attempts`
/// chooses `RetryNone`. The provider forces a sandboxed first attempt.
#[tokio::test]
async fn sandbox_denial_retries_under_none() {
    let orch = ToolOrchestrator::new(FixedSandboxProvider(SandboxType::Restricted), AutoApprover);
    let tool = DenyingTool::new(false);

    let result = orch
        .run(
            &tool,
            &REQ,
            &ctx(),
            &env(/* restricted */ true),
            AskForApproval::OnFailure,
        )
        .await
        .expect("retry under None should succeed");

    assert_eq!(result.sandbox_used, SandboxType::None);
    let seen = tool.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![SandboxType::Restricted, SandboxType::None],
        "first attempt sandboxed, retry under None"
    );
}

/// (5) A sandbox denial with `on_denial = Return` propagates the error without
/// retrying.
///
/// `Never` => `wants_no_sandbox_approval` is false, so `plan_attempts` chooses
/// `Return`.
#[tokio::test]
async fn sandbox_denial_return_propagates() {
    let orch = ToolOrchestrator::new(FixedSandboxProvider(SandboxType::Restricted), AutoApprover);
    let tool = DenyingTool::new(false);

    let err = orch
        .run(
            &tool,
            &REQ,
            &ctx(),
            &env(/* restricted */ true),
            AskForApproval::Never,
        )
        .await
        .expect_err("Return policy must propagate the denial");

    assert!(matches!(err, ToolError::Sandboxed(_)));
    let seen = tool.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![SandboxType::Restricted], "no retry under Return");
}
