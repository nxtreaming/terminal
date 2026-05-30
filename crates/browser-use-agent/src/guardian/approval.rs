//! `GuardianApprover`: wires the guardian into the EXISTING async `Approver`
//! seam (`crate::tools::runtime::Approver`, runtime.rs:130) — signature
//! UNCHANGED.
//!
//! ## Precedence (mirrors codex `approval_resolution_for_command`,
//! `codex-rs/core/src/tools/sandboxing.rs:60`)
//!
//! 1. **Cached session approval wins** — codex `SessionApprovalCache`
//!    (sandboxing.rs:44, `get`:46/`put`:54). A cached `AllowForSession`
//!    short-circuits to `ApprovedForSession` WITHOUT calling the reviewer.
//! 2. **execpolicy (Safety-2) Forbidden short-circuits** — defense in depth:
//!    a deterministic `ExecPolicyDecision::Forbidden`
//!    (`crate::execpolicy::ExecPolicyDecision`, policy.rs:69) denies BEFORE
//!    and REGARDLESS of the reviewer (a reviewer `Allow` can NEVER override a
//!    deterministic `Forbidden`).
//! 3. **Guardian review** — the LLM-reviewer gate (browser-use addition).
//!    FAIL-CLOSED: any reviewer error/timeout/open-circuit ⇒ `Denied`.
//! 4. **PermissionRequest precedence** — an `Escalate` verdict is resolved
//!    through an injected [`EscalationResolver`]. The intended production
//!    binding is the `PermissionRequest` hook flow
//!    (`crate::hooks::event::HookEvent::PermissionRequest`, event.rs:65 —
//!    the `hook.permission_request` `PendingEvent` in
//!    `hooks/runtime.rs`). Mapping: `Allow`⇒`Approved`, `Deny`/`Ask`⇒
//!    `Denied` (fail closed — no interactive human in this async, non-blocking
//!    approval context).
//!
//! The existing `Approver::review` is **async** (runtime.rs:131), and the
//! guardian's reviewer is async, so this composes directly with no
//! thread/runtime bridge — and with a fake reviewer the whole path is
//! network-free.

use async_trait::async_trait;

use crate::execpolicy::ExecPolicyDecision;
use crate::tools::runtime::{ApprovalRequest, Approver};
use crate::tools::ReviewDecision;

use super::reviewer::GuardianRequest;
use super::{GuardedDecision, Guardian};

/// How a guardian `Escalate` is resolved into a concrete decision.
///
/// PermissionRequest precedence (step 4). The default
/// ([`EscalationResolver::FailClosed`]) denies — matching the fail-closed
/// contract when there is no interactive human. Production should bind this
/// to the `PermissionRequest` hook flow (see module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationResolver {
    /// No human available ⇒ deny (fail closed). This is the default.
    FailClosed,
    /// A human/hook approved the escalation ⇒ approve once.
    HumanApproved,
}

impl EscalationResolver {
    fn resolve(self) -> ReviewDecision {
        match self {
            // `Ask`/`Deny` with no interactive human both fail closed.
            EscalationResolver::FailClosed => ReviewDecision::Denied,
            EscalationResolver::HumanApproved => ReviewDecision::Approved,
        }
    }
}

/// Approver that composes execpolicy + the guardian + escalation resolution.
///
/// `Clone` is NOT required by the `Approver` trait (runtime.rs:130 only needs
/// `Send + Sync`), but we derive it cheaply (the guardian is `Arc`-backed) so
/// callers can share one guardian.
#[derive(Clone)]
pub struct GuardianApprover {
    guardian: Guardian,
    /// Resolves escalations into a concrete decision (PermissionRequest
    /// precedence). Defaults to fail-closed.
    escalation: EscalationResolver,
    /// Optional deterministic exec-policy decision (Safety-2) injected
    /// per-construction. In a fuller wiring this would be evaluated
    /// per-invocation from the `execpolicy::Policy`; kept as an override here
    /// so the defense-in-depth precedence (Forbidden wins over reviewer Allow)
    /// is reachable + testable without touching the execpolicy module.
    policy_override: Option<ExecPolicyDecision>,
}

impl GuardianApprover {
    /// Build an approver around the given guardian (fail-closed escalation).
    pub fn new(guardian: Guardian) -> Self {
        Self {
            guardian,
            escalation: EscalationResolver::FailClosed,
            policy_override: None,
        }
    }

    /// Set how escalations resolve (PermissionRequest precedence).
    pub fn with_escalation(mut self, resolver: EscalationResolver) -> Self {
        self.escalation = resolver;
        self
    }

    /// Inject a deterministic exec-policy decision (Safety-2) that takes
    /// precedence over the reviewer (defense in depth).
    pub fn with_exec_policy(mut self, decision: ExecPolicyDecision) -> Self {
        self.policy_override = Some(decision);
        self
    }

    /// Access the underlying guardian (shares circuit + cache).
    pub fn guardian(&self) -> &Guardian {
        &self.guardian
    }
}

#[async_trait]
impl Approver for GuardianApprover {
    async fn review(&self, payload: ApprovalRequest<'_>) -> ReviewDecision {
        let tool_name = payload.ctx.tool_name.clone();
        let reason = payload.reason.clone().unwrap_or_default();
        let req = GuardianRequest::new(tool_name, reason).with_context(payload.ctx.call_id.clone());

        // (1) cached session approval wins (handled inside Guardian::review,
        //     codex sandboxing.rs:52-59). Check it first so a cached approval
        //     is honoured even ahead of an execpolicy Forbidden override
        //     (matches codex: a cached ApprovedForSession short-circuits).
        let has_session_approval = self
            .guardian
            .cache()
            .get(&req.cache_key())
            .map(|d| matches!(d, GuardedDecision::AllowForSession))
            .unwrap_or(false);

        // (2) execpolicy Forbidden short-circuits BEFORE the reviewer
        //     (defense in depth): a deterministic Forbidden can never be
        //     overridden by a reviewer Allow. FAIL CLOSED.
        if !has_session_approval {
            if let Some(ExecPolicyDecision::Forbidden { .. }) = &self.policy_override {
                return ReviewDecision::Denied;
            }
        }

        // (3) guardian review (fail-closed on error/circuit-open) and
        // (4) escalation -> PermissionRequest precedence.
        match self.guardian.review(&req).await {
            GuardedDecision::Allow => ReviewDecision::Approved,
            GuardedDecision::AllowForSession => ReviewDecision::ApprovedForSession,
            GuardedDecision::Deny { .. } => ReviewDecision::Denied,
            GuardedDecision::Escalate { .. } => self.escalation.resolve(),
        }
    }
}
