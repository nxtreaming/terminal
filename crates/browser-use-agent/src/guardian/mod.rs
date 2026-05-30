//! Guardian: a fail-closed LLM-reviewer safety gate (browser-use addition).
//!
//! ## codex parity vs. browser-use addition
//!
//! - **codex parity (mirrored):** the session-scoped approval cache and the
//!   approval precedence. codex's `SessionApprovalCache`
//!   (`codex-rs/core/src/tools/sandboxing.rs:44`, `get`:46, `put`:54) drives
//!   a command-keyed cache where a cached `ApprovedForSession` short-circuits
//!   (sandboxing.rs:52-59), and `approval_resolution_for_command`
//!   (sandboxing.rs:60) is the precedence resolver. We mirror that cache +
//!   precedence in [`SessionDecisionCache`] and
//!   [`approval::GuardianApprover`].
//! - **codex parity (loose analog):** the LLM review flow,
//!   `codex-rs/core/src/tasks/review.rs` (`ReviewTask::run`, review.rs:59) —
//!   drive a model turn, parse a structured verdict. Our
//!   [`reviewer::GuardianReviewer`] seam mirrors that shape behind an
//!   injectable trait.
//! - **browser-use ADDITION (NOT codex):** running the reviewer as a
//!   *safety gate on each gated tool call*, plus **fail-closed** semantics
//!   and the [`circuit`] breaker. The user explicitly requested this. There
//!   is no guardian / circuit-breaker / llm-review in the legacy
//!   `browser-use-core` either (verified: a recursive grep of
//!   `terminal-decodex/crates/browser-use-core/src/` for
//!   `guardian|circuit.breaker|llm.review|safety.review` returned nothing),
//!   so codex is the only authoritative analog and the gate itself is new.
//!
//! ## FAIL-CLOSED guarantee
//!
//! [`Guardian::review`] yields [`GuardedDecision::Allow`] ONLY when the
//! reviewer explicitly returns [`reviewer::GuardianVerdict::Allow`]. Every
//! other path — reviewer `Err` (timeout/failure), an open circuit, or an
//! `Escalate` verdict — resolves to `Deny`/`Escalate`, never `Allow`. The
//! single error⇒deny branch is the `Err(err) =>` arm of [`Guardian::review`].

pub mod approval;
pub mod circuit;
pub mod reviewer;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use crate::sandbox_backends::provider::PlatformSandboxProvider;
use crate::tools::runtime::AutoApprover;
use crate::tools::ToolOrchestrator;

use self::approval::GuardianApprover;
use self::circuit::CircuitBreaker;
use self::reviewer::{GuardianRequest, GuardianReviewer, GuardianVerdict};

/// A resolved, cacheable guardian decision.
///
/// Mirrors the *meaning* of codex `ReviewDecision`
/// (`codex-rs/protocol/src/protocol.rs:3530`): `Allow` ~ `Approved`,
/// `AllowForSession` ~ `ApprovedForSession`, `Deny` ~ `Denied`, `Escalate`
/// ~ human-in-the-loop. codex's `ReviewDecision` defaults to a non-approving
/// value — fail-closed by construction — and we keep that property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardedDecision {
    /// Permit for this single invocation.
    Allow,
    /// Permit for the remainder of the session (cached, short-circuits).
    AllowForSession,
    /// Deny execution with a reason.
    Deny { reason: String },
    /// Defer to a human decision with a reason.
    Escalate { reason: String },
}

impl GuardedDecision {
    /// Whether this decision is cacheable as a session-scoped approval.
    ///
    /// Mirrors codex sandboxing.rs:52-59 where only `ApprovedForSession` is
    /// honoured from the cache as an automatic re-approval.
    fn is_session_cacheable(&self) -> bool {
        matches!(self, GuardedDecision::AllowForSession)
    }
}

/// Session-scoped decision cache, mirroring codex `SessionApprovalCache`
/// (`codex-rs/core/src/tools/sandboxing.rs:44`).
///
/// codex keys on the command vector and stores `ReviewDecision`; we key on
/// the [`GuardianRequest::cache_key`] string and store [`GuardedDecision`].
/// Like codex (`get`:46 / `put`:54) it is `Clone` + interior-mutable so the
/// same cache is shared across the session.
#[derive(Debug, Default, Clone)]
pub struct SessionDecisionCache {
    approved: Arc<Mutex<HashMap<String, GuardedDecision>>>,
}

impl SessionDecisionCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// codex parity: `SessionApprovalCache::get` (sandboxing.rs:46).
    pub fn get(&self, key: &str) -> Option<GuardedDecision> {
        self.approved.lock().unwrap().get(key).cloned()
    }

    /// codex parity: `SessionApprovalCache::put` (sandboxing.rs:54). Only
    /// session-cacheable decisions are stored so that, exactly like codex
    /// (sandboxing.rs:52-59), a cached entry can only ever short-circuit to
    /// an approval — never silently cache a one-shot allow or a deny.
    pub fn put(&self, key: String, decision: GuardedDecision) {
        if decision.is_session_cacheable() {
            self.approved.lock().unwrap().insert(key, decision);
        }
    }
}

/// The guardian: reviewer + circuit breaker + session decision cache.
///
/// browser-use addition. `Clone` (cheap: the reviewer is `Arc`, the cache is
/// `Arc`-backed, the breaker is `Arc<Mutex<_>>`-wrapped) so it can be embedded
/// in a `Clone` approver.
#[derive(Clone)]
pub struct Guardian {
    reviewer: Arc<dyn GuardianReviewer>,
    circuit: Arc<Mutex<CircuitBreaker>>,
    cache: SessionDecisionCache,
}

impl Guardian {
    /// Build a guardian around the given reviewer with default breaker
    /// constants and a fresh session cache.
    pub fn new(reviewer: Arc<dyn GuardianReviewer>) -> Self {
        Self {
            reviewer,
            circuit: Arc::new(Mutex::new(CircuitBreaker::new())),
            cache: SessionDecisionCache::new(),
        }
    }

    /// Build a guardian with an explicit circuit breaker and cache (tests).
    pub fn with_parts(
        reviewer: Arc<dyn GuardianReviewer>,
        circuit: CircuitBreaker,
        cache: SessionDecisionCache,
    ) -> Self {
        Self {
            reviewer,
            circuit: Arc::new(Mutex::new(circuit)),
            cache,
        }
    }

    /// Access the shared session decision cache.
    pub fn cache(&self) -> &SessionDecisionCache {
        &self.cache
    }

    /// Review a gated tool invocation.
    ///
    /// ## FAIL-CLOSED control flow (the load-bearing safety path)
    ///
    /// 1. **Cache:** a cached `AllowForSession` short-circuits to `Allow`
    ///    WITHOUT calling the reviewer (codex sandboxing.rs:52-59).
    /// 2. **Circuit:** if the breaker does not permit a call (Open, or a
    ///    half-open trial already taken), we **return `Deny` immediately**
    ///    and DO NOT call the reviewer. (browser-use addition.)
    /// 3. **Reviewer:** otherwise we call the async reviewer.
    ///    - `Ok(Allow)`    -> record success, `Allow`.
    ///    - `Ok(Deny)`     -> record success (a *verdict* is a healthy
    ///                        reviewer), `Deny`.
    ///    - `Ok(Escalate)` -> record success, `Escalate`.
    ///    - `Err(_)`       -> record FAILURE on the breaker, then
    ///                        **`Deny`** (NEVER `Allow`). This is the single
    ///                        branch where reviewer error maps to deny.
    pub async fn review(&self, req: &GuardianRequest) -> GuardedDecision {
        let key = req.cache_key();

        // (1) cache wins — codex SessionApprovalCache precedence.
        if let Some(cached) = self.cache.get(&key) {
            if cached.is_session_cacheable() {
                return GuardedDecision::Allow;
            }
        }

        // (2) circuit gate — fail closed when not permitted.
        let now = Instant::now();
        let permitted = {
            let mut cb = self.circuit.lock().unwrap();
            cb.allows_call_at(now)
        };
        if !permitted {
            // Circuit OPEN (or trial exhausted): deny without invoking the
            // reviewer. FAIL CLOSED.
            return GuardedDecision::Deny {
                reason: "guardian circuit open: reviewer unavailable, \
                         failing closed"
                    .to_string(),
            };
        }

        // (3) ask the reviewer.
        match self.reviewer.review(req).await {
            Ok(GuardianVerdict::Allow) => {
                self.circuit.lock().unwrap().record_success();
                GuardedDecision::Allow
            }
            Ok(GuardianVerdict::Deny { reason }) => {
                // A clear deny is still a *healthy* reviewer response.
                self.circuit.lock().unwrap().record_success();
                GuardedDecision::Deny { reason }
            }
            Ok(GuardianVerdict::Escalate { reason }) => {
                self.circuit.lock().unwrap().record_success();
                GuardedDecision::Escalate { reason }
            }
            Err(err) => {
                // FAIL CLOSED: a reviewer error/timeout NEVER allows.
                self.circuit
                    .lock()
                    .unwrap()
                    .record_failure_at(Instant::now());
                GuardedDecision::Deny {
                    reason: format!("guardian reviewer error: {err}"),
                }
            }
        }
    }

    /// Cache an `AllowForSession` decision for the given request so future
    /// identical requests short-circuit (codex sandboxing.rs:52-59).
    pub fn remember_session_approval(&self, req: &GuardianRequest) {
        self.cache
            .put(req.cache_key(), GuardedDecision::AllowForSession);
    }
}

/// Build the SECURED tool orchestrator: the REAL platform sandbox provider
/// (Safety-1, [`PlatformSandboxProvider`]) + the guardian approver, behind the
/// EXISTING generic `ToolOrchestrator::new(sandbox, approver)`
/// (orchestrator.rs:75 — signature UNCHANGED).
///
/// This is the additive "secured path" the WP asks for. It does NOT mutate the
/// unsecured wiring (`ToolOrchestrator::stub()` / `NoneSandboxProvider` /
/// `AutoApprover` all stay as-is). You're ADDING a secured path.
///
/// browser-use addition: composes the guardian gate with the real sandbox.
pub fn build_secured_orchestrator(
    reviewer: Arc<dyn GuardianReviewer>,
) -> ToolOrchestrator<PlatformSandboxProvider, GuardianApprover> {
    let guardian = Guardian::new(reviewer);
    let approver = GuardianApprover::new(guardian);
    ToolOrchestrator::new(PlatformSandboxProvider, approver)
}

/// Build a secured orchestrator from an already-constructed approver. Lets
/// callers/tests share one guardian (and thus one circuit + cache) between the
/// approver and other inspection.
pub fn build_secured_orchestrator_with_approver(
    approver: GuardianApprover,
) -> ToolOrchestrator<PlatformSandboxProvider, GuardianApprover> {
    ToolOrchestrator::new(PlatformSandboxProvider, approver)
}

/// Convenience: an UNSECURED auto-approver orchestrator on the real sandbox.
/// Not used by the secured path; provided only so the real sandbox can be
/// exercised without the guardian when explicitly desired. Kept here (not in
/// `tools/`) so no existing file is edited.
pub fn build_real_sandbox_orchestrator() -> ToolOrchestrator<PlatformSandboxProvider, AutoApprover>
{
    ToolOrchestrator::new(PlatformSandboxProvider, AutoApprover)
}

#[cfg(test)]
mod tests;
