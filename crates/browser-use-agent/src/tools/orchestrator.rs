//! `ToolOrchestrator` — async driver over the pure `plan_attempts` decision table.
//!
//! Every branch here delegates to the merged pure decision fns in
//! [`super::runtime`] (`default_exec_approval_requirement`,
//! `sandbox_override_for_first_attempt`, `plan_attempts`, `map_decision`, plus
//! the `Approvable`/`Sandboxable` accessors). The async layer only performs I/O:
//! approval prompts ([`Approver`]), sandbox selection/preparation
//! ([`SandboxProvider`]), the session approval cache, and `tool.run`.
//!
//! Codex parity: `core/src/tools/orchestrator.rs` (run flow,
//! approval → select → run → escalate → retry-`None`) and
//! `core/src/tools/sandboxing.rs::with_cached_approval` (session cache).

use std::marker::PhantomData;

use tokio::sync::Mutex;

use super::approval::{ApprovalStore, AskForApproval, ReviewDecision};
use super::runtime::{
    default_exec_approval_requirement, map_decision, plan_attempts,
    sandbox_override_for_first_attempt, ApprovalRequest, Approver, AutoApprover, DenialAction,
    SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use super::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxPermissions, SandboxProvider, SandboxType,
};

/// The per-turn environment the orchestrator runs a tool inside.
///
/// Codex parity: the slice of `TurnContext` the orchestrator consults
/// (filesystem sandbox policy, managed-network state, guardian flags).
pub struct TurnEnv {
    pub file_system_sandbox_policy: FileSystemSandboxPolicy,
    pub managed_network_active: bool,
    pub strict_auto_review: bool,
    pub use_guardian: bool,
}

/// The outcome of a successful orchestrated tool call.
#[derive(Debug)]
pub struct OrchestratorRunResult<Out> {
    pub output: Out,
    /// The sandbox the (final) attempt actually ran under.
    pub sandbox_used: SandboxType,
    /// Whether this call was approved for the rest of the session.
    pub approved_for_session: bool,
}

/// Async driver over the pure attempt planner.
///
/// Generic over a [`SandboxProvider`] (the sandbox seam) and an [`Approver`]
/// (the approval seam); the session approval cache is shared behind a
/// `tokio::Mutex` (codex `ApprovalStore`).
pub struct ToolOrchestrator<S: SandboxProvider, A: Approver> {
    sandbox: S,
    approver: A,
    approvals: Mutex<ApprovalStore>,
    _s: PhantomData<(S, A)>,
}

impl ToolOrchestrator<NoneSandboxProvider, AutoApprover> {
    /// The stub: `sandbox = None` + auto-approve + empty session cache.
    pub fn stub() -> Self {
        Self {
            sandbox: NoneSandboxProvider,
            approver: AutoApprover,
            approvals: Mutex::new(ApprovalStore::default()),
            _s: PhantomData,
        }
    }
}

impl<S: SandboxProvider, A: Approver> ToolOrchestrator<S, A> {
    /// Construct an orchestrator from a sandbox provider + approver.
    pub fn new(sandbox: S, approver: A) -> Self {
        Self {
            sandbox,
            approver,
            approvals: Mutex::new(ApprovalStore::default()),
            _s: PhantomData,
        }
    }

    /// Run a single tool invocation end-to-end.
    ///
    /// Flow (codex `orchestrator.rs:58-388`):
    /// 1. Resolve the [`ExecApprovalRequirement`](super::approval::ExecApprovalRequirement):
    ///    the tool's `exec_approval_requirement` override, else
    ///    [`default_exec_approval_requirement`].
    /// 2. Compute the first-attempt [`SandboxOverride`](super::sandbox::SandboxOverride)
    ///    via [`sandbox_override_for_first_attempt`].
    /// 3. Distill the plan via [`plan_attempts`] (the pure decision table).
    /// 4. If `needs_initial_approval` and not already cached, ask the
    ///    [`Approver`] and run the verdict through [`map_decision`]; cache an
    ///    `ApprovedForSession`.
    /// 5. Pick the initial sandbox via [`SandboxProvider::select_initial`],
    ///    forced to [`SandboxType::None`] when the override bypasses it.
    /// 6. Prepare + run the tool.
    /// 7. On [`ToolError::Sandboxed`], consult [`AttemptPlan::on_denial`](super::runtime::AttemptPlan):
    ///    [`DenialAction::Return`] propagates; [`DenialAction::RetryNone`]
    ///    re-approves (if required) and re-runs under [`SandboxType::None`].
    ///
    /// The frozen signature is kept EXACT; the unused `_policy`-vs-`env`
    /// duplication is reconciled by preferring the explicit `_policy` argument.
    pub async fn run<Req, Out, T>(
        &self,
        tool: &T,
        req: &Req,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
    ) -> Result<OrchestratorRunResult<Out>, ToolError>
    where
        T: ToolRuntime<Req, Out>,
        Req: Send + Sync,
        Out: Send,
    {
        let fs = &env.file_system_sandbox_policy;
        let perms = tool.sandbox_permissions(req);

        // 1. Resolve the approval requirement (tool override else policy default).
        let requirement = tool
            .exec_approval_requirement(req)
            .unwrap_or_else(|| default_exec_approval_requirement(policy, fs));

        // 2. First-attempt sandbox override (escalated perms / full trust).
        let ovr = sandbox_override_for_first_attempt(perms, &requirement, fs);

        // 3+4. Approval gate: is this call already approved for the session?
        // Mirrors `with_cached_approval`: an `ApprovedForSession` on every key
        // lets us skip prompting.
        let keys = tool.approval_keys(req);
        let already_approved = self.all_keys_approved(&keys).await;

        let should_bypass = tool.should_bypass_approval(policy, already_approved);
        let wants_no_sandbox = tool.wants_no_sandbox_approval(policy);
        let escalate_on_failure = tool.escalate_on_failure();

        // 4. Distill the plan (pure decision table). `net_denial` is false for
        // the modeled first attempt (we have no denial yet).
        let plan = plan_attempts(
            &requirement,
            ovr,
            escalate_on_failure,
            wants_no_sandbox,
            should_bypass,
            env.strict_auto_review,
            already_approved,
            /* net_denial */ false,
        );

        let mut approved_for_session = already_approved;

        // 5. Initial approval gate. Skip prompting if already cached for session.
        if plan.needs_initial_approval && !already_approved {
            let decision = self.review(ctx).await;
            // `map_decision` (pure) -> Ok if approving, else ToolError::Rejected.
            map_decision(decision, None)?;
            if matches!(decision, ReviewDecision::ApprovedForSession) {
                self.cache_session_approval(&keys).await;
                approved_for_session = true;
            }
        }

        // 6. Select the initial sandbox. The plan's `initial_sandbox` already
        // encodes the override (None when bypassed); ask the provider for the
        // configured restricted flavor, then honor the override.
        let provider_choice =
            self.sandbox
                .select_initial(fs, tool.sandbox_preference(), env.managed_network_active);
        let initial_sandbox = match plan.initial_sandbox {
            SandboxType::None => SandboxType::None,
            SandboxType::Restricted => provider_choice,
        };

        // 7. Run under the chosen sandbox.
        match self.run_under(tool, req, ctx, initial_sandbox, perms).await {
            Ok(output) => Ok(OrchestratorRunResult {
                output,
                sandbox_used: initial_sandbox,
                approved_for_session,
            }),
            // Sandbox denial -> consult the pure `on_denial` action.
            Err(ToolError::Sandboxed(denial)) => match plan.on_denial {
                DenialAction::Return => Err(ToolError::Sandboxed(denial)),
                DenialAction::RetryNone { needs_reapproval } => {
                    if needs_reapproval {
                        let decision = self.review(ctx).await;
                        map_decision(decision, None)?;
                        if matches!(decision, ReviewDecision::ApprovedForSession) {
                            self.cache_session_approval(&keys).await;
                            approved_for_session = true;
                        }
                    }
                    let output = self
                        .run_under(tool, req, ctx, SandboxType::None, perms)
                        .await?;
                    Ok(OrchestratorRunResult {
                        output,
                        sandbox_used: SandboxType::None,
                        approved_for_session,
                    })
                }
            },
            Err(other) => Err(other),
        }
    }

    /// Prepare a sandbox launch for `sandbox` and run the tool inside it.
    async fn run_under<Req, Out, T>(
        &self,
        tool: &T,
        req: &Req,
        ctx: &ToolCtx,
        sandbox: SandboxType,
        perms: SandboxPermissions,
    ) -> Result<Out, ToolError>
    where
        T: ToolRuntime<Req, Out>,
        Req: Send + Sync,
        Out: Send,
    {
        let launch = self.sandbox.prepare(sandbox, &ctx.cwd, perms);
        let attempt = SandboxAttempt {
            sandbox: launch.sandbox,
            permissions: perms,
            enforce_managed_network: false,
            launch: &launch,
            cancel: launch.cancel.clone(),
        };
        tool.run(req, &attempt, ctx).await
    }

    /// Whether every approval key is `ApprovedForSession` in the cache.
    ///
    /// Codex parity: the `already_approved` check in `with_cached_approval`. An
    /// empty key set is never pre-approved (the call must still be gated).
    async fn all_keys_approved<K: serde::Serialize>(&self, keys: &[K]) -> bool {
        if keys.is_empty() {
            return false;
        }
        let store = self.approvals.lock().await;
        keys.iter()
            .all(|k| matches!(store.get(k), Some(ReviewDecision::ApprovedForSession)))
    }

    /// Record `ApprovedForSession` for every key (codex `with_cached_approval`).
    async fn cache_session_approval<K: serde::Serialize>(&self, keys: &[K])
    where
        K: Clone,
    {
        let mut store = self.approvals.lock().await;
        for k in keys {
            store.put(k.clone(), ReviewDecision::ApprovedForSession);
        }
    }

    /// Ask the approver for a verdict on this call.
    async fn review(&self, ctx: &ToolCtx) -> ReviewDecision {
        self.approver
            .review(ApprovalRequest {
                ctx,
                reason: None,
                guardian_review_id: None,
            })
            .await
    }
}
