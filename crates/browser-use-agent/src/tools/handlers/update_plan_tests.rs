//! Tests for the async `update_plan` tool ([`UpdatePlanTool`]).
//!
//! No network. Pure-unit coverage of the validation rules + the serde wire
//! values, plus an orchestrator-driven integration test through the
//! [`ToolRuntime`] seam (mirroring `view_image_tests` / `shell_tests`).

use super::update_plan::{
    render_plan, validate_plan, PlanItem, PlanStatus, UpdatePlanRequest, UpdatePlanTool,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

fn item(step: &str, status: PlanStatus) -> PlanItem {
    PlanItem {
        step: step.to_string(),
        status,
    }
}

/// A `SandboxType::None` launch + attempt for direct `run` calls.
fn none_launch() -> SandboxLaunch {
    SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: None,
    }
}

fn none_attempt(launch: &SandboxLaunch) -> SandboxAttempt<'_> {
    SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch,
        cancel: None,
    }
}

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "test-call".to_string(),
        tool_name: "update_plan".to_string(),
        cwd: std::env::temp_dir(),
        artifact_root: std::env::temp_dir().join("artifacts"),
    }
}

fn turn_env() -> TurnEnv {
    TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted: false,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    }
}

// ---- (4) status enum serde round-trips to the exact codex wire strings ----
//
// Codex parity: `StepStatus` is `#[serde(rename_all = "snake_case")]` over
// `Pending` / `InProgress` / `Completed` (`protocol/src/plan_tool.rs:7-13`), so
// the JSON wire strings are `"pending"` / `"in_progress"` / `"completed"`.

#[test]
fn plan_status_serializes_to_codex_wire_strings() {
    assert_eq!(
        serde_json::to_string(&PlanStatus::Pending).unwrap(),
        "\"pending\""
    );
    assert_eq!(
        serde_json::to_string(&PlanStatus::InProgress).unwrap(),
        "\"in_progress\""
    );
    assert_eq!(
        serde_json::to_string(&PlanStatus::Completed).unwrap(),
        "\"completed\""
    );
}

#[test]
fn plan_status_deserializes_from_codex_wire_strings() {
    assert_eq!(
        serde_json::from_str::<PlanStatus>("\"pending\"").unwrap(),
        PlanStatus::Pending
    );
    assert_eq!(
        serde_json::from_str::<PlanStatus>("\"in_progress\"").unwrap(),
        PlanStatus::InProgress
    );
    assert_eq!(
        serde_json::from_str::<PlanStatus>("\"completed\"").unwrap(),
        PlanStatus::Completed
    );
}

#[test]
fn plan_status_round_trips() {
    for s in [
        PlanStatus::Pending,
        PlanStatus::InProgress,
        PlanStatus::Completed,
    ] {
        let json = serde_json::to_string(&s).unwrap();
        let back: PlanStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}

#[test]
fn request_matches_codex_args_wire_shape() {
    // Mirrors codex `UpdatePlanArgs { explanation, plan: [{ step, status }] }`
    // (`protocol/src/plan_tool.rs:22-29`), with `explanation` omitted when None.
    let json = r#"{"plan":[{"step":"do a thing","status":"in_progress"}]}"#;
    let req: UpdatePlanRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.explanation, None);
    assert_eq!(req.plan.len(), 1);
    assert_eq!(req.plan[0].step, "do a thing");
    assert_eq!(req.plan[0].status, PlanStatus::InProgress);

    // explanation is skipped on serialize when None.
    let out = serde_json::to_string(&req).unwrap();
    assert!(!out.contains("explanation"), "got: {out}");
    assert!(out.contains("\"in_progress\""), "got: {out}");
}

// ---- (1) a valid plan with one in_progress + pending/completed is accepted ----

#[test]
fn accepts_valid_plan_with_one_in_progress() {
    let plan = vec![
        item("scope the work", PlanStatus::Completed),
        item("write the code", PlanStatus::InProgress),
        item("run the tests", PlanStatus::Pending),
    ];
    // Validation returns the in_progress count (1).
    assert_eq!(validate_plan(&plan).unwrap(), 1);

    let req = UpdatePlanRequest {
        explanation: Some("making progress".to_string()),
        plan,
    };
    let summary = render_plan(&req);
    assert!(summary.contains("making progress"), "got: {summary}");
    assert!(summary.contains("scope the work"), "got: {summary}");
    assert!(summary.contains("write the code"), "got: {summary}");
    assert!(summary.contains("run the tests"), "got: {summary}");
}

#[test]
fn accepts_plan_with_zero_in_progress() {
    // All pending / completed is fine (0 in_progress).
    let plan = vec![
        item("a", PlanStatus::Pending),
        item("b", PlanStatus::Completed),
    ];
    assert_eq!(validate_plan(&plan).unwrap(), 0);
}

// ---- (2) a plan with TWO in_progress is rejected (codex one-in_progress rule) -

#[test]
fn rejects_plan_with_two_in_progress() {
    // Codex parity: "At most one step can be in_progress at a time."
    // (`core/src/tools/handlers/plan_spec.rs:37`). We hard-reject.
    let plan = vec![
        item("first", PlanStatus::InProgress),
        item("second", PlanStatus::InProgress),
    ];
    let err = validate_plan(&plan).unwrap_err();
    let ToolError::Rejected(msg) = err else {
        panic!("expected Rejected, got {err:?}");
    };
    assert!(msg.contains("in_progress"), "got: {msg}");
}

// ---- (3) empty plan / empty step text is handled per codex/legacy ----

#[test]
fn rejects_empty_plan() {
    let err = validate_plan(&[]).unwrap_err();
    let ToolError::Rejected(msg) = err else {
        panic!("expected Rejected, got {err:?}");
    };
    assert!(msg.contains("at least one step"), "got: {msg}");
}

#[test]
fn rejects_blank_step_text() {
    let plan = vec![
        item("real step", PlanStatus::Pending),
        item("   ", PlanStatus::Pending),
    ];
    let err = validate_plan(&plan).unwrap_err();
    let ToolError::Rejected(msg) = err else {
        panic!("expected Rejected, got {err:?}");
    };
    assert!(msg.contains("empty step text"), "got: {msg}");
}

// ---- accessors + parallel-safety (matches codex) ----

#[test]
fn approval_accessors() {
    let tool = UpdatePlanTool::new();
    let req = UpdatePlanRequest::from_items([("a", PlanStatus::Pending)]);
    assert_eq!(tool.approval_keys(&req).len(), 1, "one key per call");
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
    // No tool-intrinsic approval requirement: defers to the policy default.
    assert!(tool.exec_approval_requirement(&req).is_none());
}

#[test]
fn update_plan_is_not_parallel_safe_matches_codex() {
    // Codex's plan handler does not override `supports_parallel_tool_calls`, so
    // it inherits the `ToolExecutor` default of `false`
    // (`codex-rs/tools/src/tool_executor.rs:51-53`). We match that.
    let tool = UpdatePlanTool::new();
    let req = UpdatePlanRequest::from_items([("a", PlanStatus::Pending)]);
    assert!(
        !tool.parallel_safe(&req),
        "update_plan must match codex: NOT parallel-safe (trait default false)"
    );
}

// Direct runtime call: a valid plan yields exit 0 with a rendered summary.
#[tokio::test]
async fn runs_valid_plan_directly() {
    let tool = UpdatePlanTool::new();
    let ctx = ctx();
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let req = UpdatePlanRequest::from_items([("only step", PlanStatus::InProgress)]);

    let out = tool.run(&req, &attempt, &ctx).await.unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.stderr.is_empty());
    assert!(out.stdout.contains("Plan updated"), "got: {}", out.stdout);
    assert!(out.stdout.contains("only step"), "got: {}", out.stdout);
}

// ---- (5) drive a valid call through the orchestrator over the seam ----

#[tokio::test]
async fn orchestrated_plan_completes_under_none() {
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = UpdatePlanTool::new();
    let ctx = ctx();
    let req = UpdatePlanRequest::from_items([
        ("design the API", PlanStatus::Completed),
        ("implement it", PlanStatus::InProgress),
        ("ship it", PlanStatus::Pending),
    ]);

    // `Never` => no approval prompt for a benign state-echo tool.
    let result = orch
        .run(&tool, &req, &ctx, &turn_env(), AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(
        result.output.stdout.contains("Plan updated"),
        "got: {}",
        result.output.stdout
    );
    assert!(
        result.output.stdout.contains("design the API"),
        "got: {}",
        result.output.stdout
    );
    assert!(
        result.output.stdout.contains("implement it"),
        "got: {}",
        result.output.stdout
    );
    assert!(
        result.output.stdout.contains("ship it"),
        "got: {}",
        result.output.stdout
    );
}

#[tokio::test]
async fn orchestrated_two_in_progress_is_rejected() {
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = UpdatePlanTool::new();
    let ctx = ctx();
    let req = UpdatePlanRequest::from_items([
        ("a", PlanStatus::InProgress),
        ("b", PlanStatus::InProgress),
    ]);

    let err = orch
        .run(&tool, &req, &ctx, &turn_env(), AskForApproval::Never)
        .await
        .expect_err("two in_progress must not complete through the orchestrator");
    assert!(
        matches!(err, ToolError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );
}
