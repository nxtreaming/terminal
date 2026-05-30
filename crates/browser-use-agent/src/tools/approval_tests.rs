//! Pure unit tests for the approval seam (codex `sandboxing.rs` parity).

use crate::tools::approval::{
    ApprovalStore, AskForApproval, GranularApprovalConfig, ReviewDecision,
};
use crate::tools::runtime::{should_bypass_approval, wants_no_sandbox_approval};

const NON_NEVER: [AskForApproval; 3] = [
    AskForApproval::UnlessTrusted,
    AskForApproval::OnFailure,
    AskForApproval::OnRequest,
];

// ---- should_bypass_approval truth table -------------------------------------

#[test]
fn never_always_bypasses() {
    assert!(should_bypass_approval(AskForApproval::Never, false));
    assert!(should_bypass_approval(AskForApproval::Never, true));
}

#[test]
fn already_approved_bypasses_for_any_policy() {
    for policy in [
        AskForApproval::UnlessTrusted,
        AskForApproval::OnFailure,
        AskForApproval::OnRequest,
        AskForApproval::Never,
        AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: false,
        }),
    ] {
        assert!(
            should_bypass_approval(policy, true),
            "policy {policy:?} with cached approval should bypass"
        );
    }
}

#[test]
fn non_never_without_approval_does_not_bypass() {
    for policy in NON_NEVER {
        assert!(
            !should_bypass_approval(policy, false),
            "policy {policy:?} without approval should NOT bypass"
        );
    }
    assert!(!should_bypass_approval(
        AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: true,
        }),
        false
    ));
}

// ---- wants_no_sandbox_approval ----------------------------------------------

#[test]
fn escalation_only_for_on_failure_and_unless_trusted() {
    assert!(wants_no_sandbox_approval(AskForApproval::OnFailure));
    assert!(wants_no_sandbox_approval(AskForApproval::UnlessTrusted));
    assert!(!wants_no_sandbox_approval(AskForApproval::OnRequest));
    assert!(!wants_no_sandbox_approval(AskForApproval::Never));
}

#[test]
fn granular_no_sandbox_approval_follows_config() {
    assert!(wants_no_sandbox_approval(AskForApproval::Granular(
        GranularApprovalConfig {
            sandbox_approval: true,
        }
    )));
    assert!(!wants_no_sandbox_approval(AskForApproval::Granular(
        GranularApprovalConfig {
            sandbox_approval: false,
        }
    )));
}

// ---- ReviewDecision::is_approved --------------------------------------------

#[test]
fn is_approved_truth_table() {
    assert!(ReviewDecision::Approved.is_approved());
    assert!(ReviewDecision::ApprovedForSession.is_approved());
    assert!(!ReviewDecision::Denied.is_approved());
    assert!(!ReviewDecision::Abort.is_approved());
    assert!(!ReviewDecision::TimedOut.is_approved());
}

// ---- ApprovalStore get/put (serde key) + session caching --------------------

#[test]
fn store_starts_empty() {
    let store = ApprovalStore::default();
    assert_eq!(store.get(&"shell:abc"), None);
}

#[test]
fn put_session_approval_is_cached() {
    let mut store = ApprovalStore::default();
    store.put("shell:abc", ReviewDecision::ApprovedForSession);
    assert_eq!(
        store.get(&"shell:abc"),
        Some(ReviewDecision::ApprovedForSession)
    );
}

#[test]
fn put_one_shot_approval_is_cached() {
    let mut store = ApprovalStore::default();
    store.put("shell:abc", ReviewDecision::Approved);
    assert_eq!(store.get(&"shell:abc"), Some(ReviewDecision::Approved));
}

#[test]
fn put_overwrites_previous_decision() {
    let mut store = ApprovalStore::default();
    store.put("shell:abc", ReviewDecision::Approved);
    store.put("shell:abc", ReviewDecision::Denied);
    assert_eq!(store.get(&"shell:abc"), Some(ReviewDecision::Denied));
}

#[test]
fn distinct_keys_do_not_collide() {
    let mut store = ApprovalStore::default();
    store.put("shell:abc", ReviewDecision::ApprovedForSession);

    assert_eq!(
        store.get(&"shell:abc"),
        Some(ReviewDecision::ApprovedForSession)
    );
    assert_eq!(store.get(&"shell:xyz"), None);
    assert_eq!(store.get(&"patch:abc"), None);
}

#[test]
fn structured_serde_keys_round_trip() {
    // Approval keys are arbitrary `Serialize` values; a tuple key works just
    // like codex's apply_patch per-path keys.
    let mut store = ApprovalStore::default();
    let key = ("apply_patch", "/repo/src/lib.rs");
    store.put(key, ReviewDecision::ApprovedForSession);

    assert_eq!(store.get(&key), Some(ReviewDecision::ApprovedForSession));
    assert_eq!(
        store.get(&("apply_patch", "/repo/src/main.rs")),
        None,
        "a different path must not be considered approved"
    );
}
