//! Network-free guardian tests with a FAKE reviewer (no real model call).
//!
//! Every test injects a fake [`GuardianReviewer`]. None touch the network or
//! a real sandbox escape.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::approval::{EscalationResolver, GuardianApprover};
use super::circuit::CircuitBreaker;
use super::reviewer::{GuardianRequest, GuardianReviewer, GuardianVerdict, ReviewerError};
use super::{
    build_secured_orchestrator, build_secured_orchestrator_with_approver, GuardedDecision,
    Guardian, SessionDecisionCache,
};

use crate::execpolicy::ExecPolicyDecision;
use crate::sandbox_backends::provider::PlatformSandboxProvider;
use crate::tools::runtime::{ApprovalRequest, Approver, ToolCtx};
use crate::tools::ReviewDecision;

// ---------------------------------------------------------------------------
// Fake reviewers (in-memory, network-free).
// ---------------------------------------------------------------------------

/// Always returns the configured verdict; counts invocations.
struct FixedReviewer {
    verdict: GuardianVerdict,
    calls: Arc<AtomicU32>,
}

impl FixedReviewer {
    fn new(verdict: GuardianVerdict) -> (Arc<Self>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let me = Arc::new(Self {
            verdict,
            calls: calls.clone(),
        });
        (me, calls)
    }
}

#[async_trait]
impl GuardianReviewer for FixedReviewer {
    async fn review(&self, _req: &GuardianRequest) -> Result<GuardianVerdict, ReviewerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.verdict.clone())
    }
}

/// Always errors (simulates timeout/failure); counts invocations.
struct ErroringReviewer {
    calls: Arc<AtomicU32>,
}

impl ErroringReviewer {
    fn new() -> (Arc<Self>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let me = Arc::new(Self {
            calls: calls.clone(),
        });
        (me, calls)
    }
}

#[async_trait]
impl GuardianReviewer for ErroringReviewer {
    async fn review(&self, _req: &GuardianRequest) -> Result<GuardianVerdict, ReviewerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ReviewerError::Timeout)
    }
}

fn req() -> GuardianRequest {
    GuardianRequest::new("shell", "{\"cmd\":\"ls\"}")
}

// ---------------------------------------------------------------------------
// FAIL-CLOSED: reviewer error/timeout => Deny (NOT Allow).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fail_closed_reviewer_error_denies() {
    let (reviewer, calls) = ErroringReviewer::new();
    let guardian = Guardian::new(reviewer);
    let decision = guardian.review(&req()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "reviewer was invoked");
    match &decision {
        GuardedDecision::Deny { .. } => {}
        other => panic!("expected fail-closed Deny, got {other:?}"),
    }
    assert_ne!(
        decision,
        GuardedDecision::Allow,
        "a reviewer error must NEVER allow"
    );
}

#[tokio::test]
async fn escalate_verdict_propagates() {
    let (reviewer, _) = FixedReviewer::new(GuardianVerdict::Escalate {
        reason: "uncertain".to_string(),
    });
    let guardian = Guardian::new(reviewer);
    assert!(matches!(
        guardian.review(&req()).await,
        GuardedDecision::Escalate { .. }
    ));
}

// ---------------------------------------------------------------------------
// CIRCUIT BREAKER: N consecutive failures => OPEN => deny without invoking
// the reviewer; after cooldown half-open success closes it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn circuit_opens_and_denies_without_invoking_reviewer() {
    let (reviewer, calls) = ErroringReviewer::new();
    // Threshold 3, long cooldown so it stays open within the test.
    let cb = CircuitBreaker::with_config(3, Duration::from_secs(3600));
    let guardian = Guardian::with_parts(reviewer, cb, SessionDecisionCache::new());

    // 3 consecutive failures trip the breaker.
    for _ in 0..3 {
        assert!(matches!(
            guardian.review(&req()).await,
            GuardedDecision::Deny { .. }
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3, "reviewer called 3x");

    // Circuit now OPEN: subsequent calls deny WITHOUT invoking reviewer.
    let decision = guardian.review(&req()).await;
    assert!(matches!(decision, GuardedDecision::Deny { .. }));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "reviewer must NOT be invoked while circuit is open"
    );
}

/// Switchable reviewer: errors while `fail` is set, then allows.
struct SwitchableReviewer {
    fail: AtomicBool,
    calls: Arc<AtomicU32>,
}

#[async_trait]
impl GuardianReviewer for SwitchableReviewer {
    async fn review(&self, _req: &GuardianRequest) -> Result<GuardianVerdict, ReviewerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(ReviewerError::Failed {
                message: "boom".to_string(),
            })
        } else {
            Ok(GuardianVerdict::Allow)
        }
    }
}

#[tokio::test]
async fn circuit_half_open_success_closes() {
    let calls = Arc::new(AtomicU32::new(0));
    let reviewer = Arc::new(SwitchableReviewer {
        fail: AtomicBool::new(true),
        calls: calls.clone(),
    });
    // Threshold 2, zero cooldown so the breaker is immediately half-open.
    let cb = CircuitBreaker::with_config(2, Duration::from_secs(0));
    let guardian = Guardian::with_parts(reviewer.clone(), cb, SessionDecisionCache::new());

    // Two failures trip it.
    assert!(matches!(
        guardian.review(&req()).await,
        GuardedDecision::Deny { .. }
    ));
    assert!(matches!(
        guardian.review(&req()).await,
        GuardedDecision::Deny { .. }
    ));

    // Cooldown is zero => half-open => a trial call is permitted. Flip the
    // reviewer to succeed; the trial should close the circuit.
    reviewer.fail.store(false, Ordering::SeqCst);
    assert_eq!(guardian.review(&req()).await, GuardedDecision::Allow);

    // Circuit closed now: another allow goes through normally.
    assert_eq!(guardian.review(&req()).await, GuardedDecision::Allow);
}

// ---------------------------------------------------------------------------
// CACHE PRECEDENCE: cached AllowForSession short-circuits (reviewer NOT
// called again); mirrors codex SessionApprovalCache.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cached_session_approval_short_circuits_reviewer() {
    // A reviewer that would DENY if ever consulted — proving the cache wins.
    let (reviewer, calls) = FixedReviewer::new(GuardianVerdict::Deny {
        reason: "would deny".to_string(),
    });
    let guardian = Guardian::new(reviewer);

    // Seed a session approval for this request.
    guardian.remember_session_approval(&req());

    let decision = guardian.review(&req()).await;
    assert_eq!(
        decision,
        GuardedDecision::Allow,
        "cached session approval must short-circuit to Allow"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "reviewer must NOT be invoked when a session approval is cached"
    );
}

#[test]
fn cache_only_stores_session_decisions() {
    let cache = SessionDecisionCache::new();
    // Non-session decisions are NOT cached (mirrors codex: only
    // ApprovedForSession is honoured from the cache).
    cache.put(
        "k".to_string(),
        GuardedDecision::Deny {
            reason: "x".to_string(),
        },
    );
    cache.put("k".to_string(), GuardedDecision::Allow);
    assert_eq!(cache.get("k"), None);

    cache.put("k".to_string(), GuardedDecision::AllowForSession);
    assert_eq!(cache.get("k"), Some(GuardedDecision::AllowForSession));
}

// ---------------------------------------------------------------------------
// APPROVER COMPOSITION + mapping (the EXISTING async Approver seam).
// ---------------------------------------------------------------------------

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "call-1".to_string(),
        tool_name: "shell".to_string(),
        cwd: PathBuf::from("/tmp"),
        artifact_root: PathBuf::from("/tmp/artifacts"),
    }
}

async fn approve(approver: &GuardianApprover) -> ReviewDecision {
    let ctx = ctx();
    let request = ApprovalRequest {
        ctx: &ctx,
        reason: None,
        guardian_review_id: None,
    };
    approver.review(request).await
}

#[tokio::test]
async fn approver_maps_allow_to_approved() {
    let (reviewer, _) = FixedReviewer::new(GuardianVerdict::Allow);
    let approver = GuardianApprover::new(Guardian::new(reviewer));
    assert_eq!(approve(&approver).await, ReviewDecision::Approved);
}

#[tokio::test]
async fn approver_maps_deny_to_denied() {
    let (reviewer, _) = FixedReviewer::new(GuardianVerdict::Deny {
        reason: "nope".to_string(),
    });
    let approver = GuardianApprover::new(Guardian::new(reviewer));
    assert_eq!(approve(&approver).await, ReviewDecision::Denied);
}

#[tokio::test]
async fn approver_error_fails_closed_to_denied() {
    let (reviewer, _) = ErroringReviewer::new();
    let approver = GuardianApprover::new(Guardian::new(reviewer));
    assert_eq!(
        approve(&approver).await,
        ReviewDecision::Denied,
        "reviewer error MUST fail closed to Denied through the approver"
    );
}

#[tokio::test]
async fn approver_escalate_fails_closed_by_default() {
    let (reviewer, _) = FixedReviewer::new(GuardianVerdict::Escalate {
        reason: "ask a human".to_string(),
    });
    let approver = GuardianApprover::new(Guardian::new(reviewer));
    // Default escalation resolver fails closed.
    assert_eq!(approve(&approver).await, ReviewDecision::Denied);
}

#[tokio::test]
async fn approver_escalate_with_human_approves() {
    let (reviewer, _) = FixedReviewer::new(GuardianVerdict::Escalate {
        reason: "ask a human".to_string(),
    });
    let approver = GuardianApprover::new(Guardian::new(reviewer))
        .with_escalation(EscalationResolver::HumanApproved);
    assert_eq!(
        approve(&approver).await,
        ReviewDecision::Approved,
        "a human/hook-approved escalation resolves to Approved"
    );
}

#[tokio::test]
async fn execpolicy_forbidden_wins_over_reviewer_allow() {
    // Defense in depth: deterministic Forbidden beats a reviewer Allow.
    let (reviewer, calls) = FixedReviewer::new(GuardianVerdict::Allow);
    let approver = GuardianApprover::new(Guardian::new(reviewer)).with_exec_policy(
        ExecPolicyDecision::Forbidden {
            reason: "rm -rf /".to_string(),
        },
    );
    assert_eq!(approve(&approver).await, ReviewDecision::Denied);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "execpolicy Forbidden short-circuits BEFORE the reviewer is called"
    );
}

// ---------------------------------------------------------------------------
// SECURED ORCHESTRATOR BUILD: real sandbox provider + guardian approver.
// ---------------------------------------------------------------------------

#[test]
fn secured_orchestrator_builds_with_platform_provider() {
    // Type-level proof that the build wires PlatformSandboxProvider +
    // GuardianApprover behind the existing generic ToolOrchestrator::new.
    let (reviewer, _) = FixedReviewer::new(GuardianVerdict::Allow);
    let _orchestrator: crate::tools::ToolOrchestrator<PlatformSandboxProvider, GuardianApprover> =
        build_secured_orchestrator(reviewer);

    let (reviewer2, _) = FixedReviewer::new(GuardianVerdict::Allow);
    let approver = GuardianApprover::new(Guardian::new(reviewer2));
    let _orchestrator2: crate::tools::ToolOrchestrator<PlatformSandboxProvider, GuardianApprover> =
        build_secured_orchestrator_with_approver(approver);
}

#[tokio::test]
async fn secured_orchestrator_approver_allows_through_the_seam() {
    // The guardian approver inside the secured build approves an Allow verdict
    // via the EXACT async Approver call the orchestrator makes — with the fake
    // reviewer (no real model, no real sandbox escape).
    let (reviewer, _) = FixedReviewer::new(GuardianVerdict::Allow);
    let approver = GuardianApprover::new(Guardian::new(reviewer));
    // Confirm the secured orchestrator builds with this approver type.
    let _orchestrator = build_secured_orchestrator_with_approver(approver.clone());

    assert_eq!(approve(&approver).await, ReviewDecision::Approved);
}

#[tokio::test]
async fn secured_orchestrator_approver_denies_on_reviewer_error() {
    let (reviewer, _) = ErroringReviewer::new();
    let approver = GuardianApprover::new(Guardian::new(reviewer));
    let _orchestrator = build_secured_orchestrator_with_approver(approver.clone());

    assert_eq!(
        approve(&approver).await,
        ReviewDecision::Denied,
        "fail-closed: a reviewer error denies through the secured approver"
    );
}
