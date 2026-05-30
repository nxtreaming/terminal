//! Pure unit tests for the sandbox seam + pure tool decisions
//! (codex `sandboxing.rs` / `orchestrator.rs` parity).

use std::path::Path;

use crate::tools::approval::{AskForApproval, ExecApprovalRequirement, ReviewDecision};
use crate::tools::runtime::{
    build_denial_reason, default_exec_approval_requirement, map_decision, plan_attempts,
    sandbox_override_for_first_attempt, AttemptPlan, DenialAction, ToolError,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxOverride, SandboxPermissions,
    SandboxPreference, SandboxProvider, SandboxType,
};

fn unrestricted_fs() -> FileSystemSandboxPolicy {
    FileSystemSandboxPolicy {
        restricted: false,
        denied_read: false,
    }
}

fn restricted_fs() -> FileSystemSandboxPolicy {
    FileSystemSandboxPolicy {
        restricted: true,
        denied_read: false,
    }
}

fn denied_read_fs() -> FileSystemSandboxPolicy {
    FileSystemSandboxPolicy {
        restricted: true,
        denied_read: true,
    }
}

// ---- NoneSandboxProvider: always None / unsandboxed -------------------------

#[test]
fn none_provider_selects_no_sandbox() {
    let provider = NoneSandboxProvider;
    for fs in [unrestricted_fs(), restricted_fs(), denied_read_fs()] {
        for pref in [SandboxPreference::Auto, SandboxPreference::Never] {
            for managed_network in [false, true] {
                assert_eq!(
                    provider.select_initial(&fs, pref, managed_network),
                    SandboxType::None
                );
            }
        }
    }
}

#[test]
fn none_provider_prepares_unsandboxed_launch() {
    let provider = NoneSandboxProvider;
    let launch = provider.prepare(
        SandboxType::Restricted,
        Path::new("/tmp"),
        SandboxPermissions::UseDefault,
    );
    assert_eq!(launch.sandbox, SandboxType::None);
    assert!(launch.cancel.is_none());
}

// ---- default_exec_approval_requirement per policy ---------------------------

#[test]
fn requirement_never_and_on_failure_skip() {
    let fs = restricted_fs();
    assert_eq!(
        default_exec_approval_requirement(AskForApproval::Never, &fs),
        ExecApprovalRequirement::Skip {
            bypass_sandbox: false
        }
    );
    assert_eq!(
        default_exec_approval_requirement(AskForApproval::OnFailure, &fs),
        ExecApprovalRequirement::Skip {
            bypass_sandbox: false
        }
    );
}

#[test]
fn requirement_on_request_depends_on_restriction() {
    assert_eq!(
        default_exec_approval_requirement(AskForApproval::OnRequest, &restricted_fs()),
        ExecApprovalRequirement::NeedsApproval { reason: None }
    );
    assert_eq!(
        default_exec_approval_requirement(AskForApproval::OnRequest, &unrestricted_fs()),
        ExecApprovalRequirement::Skip {
            bypass_sandbox: false
        }
    );
}

#[test]
fn requirement_unless_trusted_always_needs_approval() {
    assert_eq!(
        default_exec_approval_requirement(AskForApproval::UnlessTrusted, &unrestricted_fs()),
        ExecApprovalRequirement::NeedsApproval { reason: None }
    );
    assert_eq!(
        default_exec_approval_requirement(AskForApproval::UnlessTrusted, &restricted_fs()),
        ExecApprovalRequirement::NeedsApproval { reason: None }
    );
}

#[test]
fn requirement_granular_disabled_is_forbidden_when_approval_needed() {
    use crate::tools::approval::GranularApprovalConfig;
    let disabled = AskForApproval::Granular(GranularApprovalConfig {
        sandbox_approval: false,
    });
    // Restricted + disabled granular sandbox approval => Forbidden.
    assert!(matches!(
        default_exec_approval_requirement(disabled, &restricted_fs()),
        ExecApprovalRequirement::Forbidden { .. }
    ));
    // Unrestricted => approval not needed => Skip (no forbidding).
    assert_eq!(
        default_exec_approval_requirement(disabled, &unrestricted_fs()),
        ExecApprovalRequirement::Skip {
            bypass_sandbox: false
        }
    );

    // Enabled granular sandbox approval, restricted => NeedsApproval.
    let enabled = AskForApproval::Granular(GranularApprovalConfig {
        sandbox_approval: true,
    });
    assert_eq!(
        default_exec_approval_requirement(enabled, &restricted_fs()),
        ExecApprovalRequirement::NeedsApproval { reason: None }
    );
}

// ---- sandbox_override_for_first_attempt -------------------------------------

#[test]
fn override_skip_bypass_true_bypasses() {
    let req = ExecApprovalRequirement::Skip {
        bypass_sandbox: true,
    };
    // Even with default perms, an explicit full-trust Skip bypasses.
    assert_eq!(
        sandbox_override_for_first_attempt(SandboxPermissions::UseDefault, &req, &restricted_fs()),
        SandboxOverride::BypassSandboxFirstAttempt
    );
}

#[test]
fn override_denied_read_suppresses_escalation() {
    let req = ExecApprovalRequirement::Skip {
        bypass_sandbox: false,
    };
    // Escalated perms would normally bypass, but deny-read suppresses it.
    assert_eq!(
        sandbox_override_for_first_attempt(
            SandboxPermissions::RequireEscalated,
            &req,
            &denied_read_fs()
        ),
        SandboxOverride::NoOverride
    );
}

#[test]
fn override_escalated_permissions_bypass() {
    let req = ExecApprovalRequirement::Skip {
        bypass_sandbox: false,
    };
    assert_eq!(
        sandbox_override_for_first_attempt(
            SandboxPermissions::RequireEscalated,
            &req,
            &restricted_fs()
        ),
        SandboxOverride::BypassSandboxFirstAttempt
    );
}

#[test]
fn override_plain_is_no_override() {
    let req = ExecApprovalRequirement::Skip {
        bypass_sandbox: false,
    };
    assert_eq!(
        sandbox_override_for_first_attempt(SandboxPermissions::UseDefault, &req, &restricted_fs()),
        SandboxOverride::NoOverride
    );
    assert_eq!(
        sandbox_override_for_first_attempt(
            SandboxPermissions::WithAdditionalPermissions,
            &req,
            &restricted_fs()
        ),
        SandboxOverride::NoOverride
    );
}

// ---- map_decision over every ReviewDecision ---------------------------------

#[test]
fn map_decision_approved_is_ok() {
    // `ToolError` carries `anyhow::Error` (not `PartialEq`), so match on `Ok`.
    assert!(map_decision(ReviewDecision::Approved, None).is_ok());
    assert!(map_decision(ReviewDecision::ApprovedForSession, None).is_ok());
}

#[test]
fn map_decision_denied_abort_timeout_are_rejected() {
    for d in [
        ReviewDecision::Denied,
        ReviewDecision::Abort,
        ReviewDecision::TimedOut,
    ] {
        match map_decision(d, None) {
            Err(ToolError::Rejected(_)) => {}
            other => panic!("expected Rejected for {d:?}, got {other:?}"),
        }
    }
}

#[test]
fn map_decision_weaves_guardian_review_id() {
    match map_decision(ReviewDecision::Denied, Some("rev-123")) {
        Err(ToolError::Rejected(msg)) => assert!(msg.contains("rev-123"), "msg: {msg}"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

// ---- build_denial_reason ----------------------------------------------------

#[test]
fn denial_reason_calls_out_host() {
    let net = build_denial_reason(Some("example.com"));
    assert!(net.contains("example.com"), "reason: {net}");
    assert!(net.contains("Network"), "reason: {net}");

    let fs = build_denial_reason(None);
    assert!(!fs.contains("Network access"), "reason: {fs}");
    assert!(fs.contains("sandbox"), "reason: {fs}");
}

// ---- plan_attempts state machine across the matrix --------------------------

fn skip() -> ExecApprovalRequirement {
    ExecApprovalRequirement::Skip {
        bypass_sandbox: false,
    }
}
fn needs() -> ExecApprovalRequirement {
    ExecApprovalRequirement::NeedsApproval { reason: None }
}
fn forbidden() -> ExecApprovalRequirement {
    ExecApprovalRequirement::Forbidden {
        reason: "no".to_string(),
    }
}

#[test]
fn plan_skip_no_strict_runs_unsandboxed_when_bypass() {
    // Skip + bypass override + not strict => no approval, sandbox None, no escalation.
    let plan = plan_attempts(
        &skip(),
        SandboxOverride::BypassSandboxFirstAttempt,
        /*escalate*/ true,
        /*wants_no_sandbox*/ true,
        /*should_bypass*/ true,
        /*strict*/ false,
        /*already_approved*/ false,
        /*net_denial*/ false,
    );
    assert_eq!(
        plan,
        AttemptPlan {
            needs_initial_approval: false,
            initial_sandbox: SandboxType::None,
            on_denial: DenialAction::RetryNone {
                needs_reapproval: false
            },
        }
    );
}

#[test]
fn plan_skip_strict_requires_initial_approval() {
    let plan = plan_attempts(
        &skip(),
        SandboxOverride::NoOverride,
        true,
        true,
        false, // not bypassable
        /*strict*/ true,
        false,
        false,
    );
    assert!(plan.needs_initial_approval);
    assert_eq!(plan.initial_sandbox, SandboxType::Restricted);
    // Strict => unsandboxed retry always re-approves.
    assert_eq!(
        plan.on_denial,
        DenialAction::RetryNone {
            needs_reapproval: true
        }
    );
}

#[test]
fn plan_needs_approval_always_gates() {
    let plan = plan_attempts(
        &needs(),
        SandboxOverride::NoOverride,
        true,
        true,
        false,
        false,
        false,
        false,
    );
    assert!(plan.needs_initial_approval);
    assert_eq!(plan.initial_sandbox, SandboxType::Restricted);
}

#[test]
fn plan_forbidden_gates() {
    let plan = plan_attempts(
        &forbidden(),
        SandboxOverride::NoOverride,
        true,
        true,
        false,
        false,
        false,
        false,
    );
    assert!(plan.needs_initial_approval);
}

#[test]
fn plan_no_escalate_returns_on_denial() {
    let plan = plan_attempts(
        &skip(),
        SandboxOverride::NoOverride,
        /*escalate*/ false,
        true,
        true,
        false,
        false,
        false,
    );
    assert_eq!(plan.on_denial, DenialAction::Return);
}

#[test]
fn plan_no_wants_no_sandbox_returns_on_denial() {
    let plan = plan_attempts(
        &skip(),
        SandboxOverride::NoOverride,
        true,
        /*wants_no_sandbox*/ false,
        true,
        false,
        false,
        false,
    );
    assert_eq!(plan.on_denial, DenialAction::Return);
}

#[test]
fn plan_network_denial_forces_reapproval() {
    // Bypassable approval would normally skip the retry prompt, but a network
    // denial always re-prompts.
    let plan = plan_attempts(
        &skip(),
        SandboxOverride::NoOverride,
        true,
        true,
        /*should_bypass*/ true,
        false,
        false,
        /*net_denial*/ true,
    );
    assert_eq!(
        plan.on_denial,
        DenialAction::RetryNone {
            needs_reapproval: true
        }
    );
}

#[test]
fn plan_bypassable_non_network_skips_retry_approval() {
    let plan = plan_attempts(
        &skip(),
        SandboxOverride::NoOverride,
        true,
        true,
        /*should_bypass*/ true,
        /*strict*/ false,
        false,
        /*net_denial*/ false,
    );
    assert_eq!(
        plan.on_denial,
        DenialAction::RetryNone {
            needs_reapproval: false
        }
    );
}

#[test]
fn plan_non_bypassable_requires_retry_approval() {
    let plan = plan_attempts(
        &skip(),
        SandboxOverride::NoOverride,
        true,
        true,
        /*should_bypass*/ false,
        false,
        false,
        false,
    );
    assert_eq!(
        plan.on_denial,
        DenialAction::RetryNone {
            needs_reapproval: true
        }
    );
}

#[test]
fn plan_initial_sandbox_follows_override() {
    let bypass = plan_attempts(
        &skip(),
        SandboxOverride::BypassSandboxFirstAttempt,
        true,
        true,
        true,
        false,
        false,
        false,
    );
    assert_eq!(bypass.initial_sandbox, SandboxType::None);

    let no_override = plan_attempts(
        &skip(),
        SandboxOverride::NoOverride,
        true,
        true,
        true,
        false,
        false,
        false,
    );
    assert_eq!(no_override.initial_sandbox, SandboxType::Restricted);
}
