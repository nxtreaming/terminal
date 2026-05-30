//! Tests for the async shell/exec tool ([`ShellTool`]).
//!
//! All tests use local process execution (no network). They drive the tool
//! directly through the [`ToolRuntime::run`] seam (with a `SandboxType::None`
//! attempt) and through the [`ToolOrchestrator`] for the approval/denylist path.

use std::collections::HashMap;

use super::shell::{
    dangerous_command_rejection, ShellRequest, ShellTool, DEFAULT_SHELL_COMMAND_TIMEOUT_MS,
    MAX_STREAM_OUTPUT_BYTES, TIMEOUT_EXIT_CODE,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

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

/// A minimal ctx rooted at a given cwd.
fn ctx_in(cwd: &std::path::Path) -> ToolCtx {
    ToolCtx {
        call_id: "test-call".to_string(),
        tool_name: "shell".to_string(),
        cwd: cwd.to_path_buf(),
    }
}

/// Run a shell request directly through the runtime (no orchestrator).
async fn run_direct(req: &ShellRequest, ctx: &ToolCtx) -> Result<ExecOutput, ToolError> {
    let tool = ShellTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, ctx).await
}

// (1) `echo hello` -> exit 0, stdout contains "hello".
#[tokio::test]
async fn echo_hello_exit_zero_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["echo", "hello"]);

    let out = run_direct(&req, &ctx).await.expect("echo should succeed");
    assert_eq!(out.exit_code, 0, "echo should exit 0");
    assert!(
        out.stdout.contains("hello"),
        "stdout should contain 'hello', got: {:?}",
        out.stdout
    );
    assert!(out.stderr.is_empty(), "stderr should be empty");
}

// (2) A command that exits non-zero -> captured exit code.
#[tokio::test]
async fn nonzero_exit_code_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["sh", "-c", "exit 7"]);

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert_eq!(out.exit_code, 7, "exit code 7 should be captured");
}

// (3) A command that sleeps longer than a short timeout -> times out.
//     Per our design (and codex exec.rs:746-748), this surfaces as an
//     `ExecOutput` carrying exit code 124.
#[tokio::test]
async fn long_command_times_out_with_124() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let mut req = ShellRequest::from_argv(["sh", "-c", "sleep 5"]);
    req.timeout_ms = Some(150); // much shorter than the 5s sleep

    let out = run_direct(&req, &ctx)
        .await
        .expect("timeout yields ExecOutput");
    assert_eq!(
        out.exit_code, TIMEOUT_EXIT_CODE,
        "timeout must report exit code 124"
    );
    assert_eq!(
        TIMEOUT_EXIT_CODE, 124,
        "parity: codex EXEC_TIMEOUT_EXIT_CODE"
    );
    assert!(
        out.stderr.contains("timed out"),
        "stderr should note the timeout, got: {:?}",
        out.stderr
    );
}

// (4) Output exceeding the byte cap -> truncated with a marker.
#[tokio::test]
async fn oversized_output_is_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let want = MAX_STREAM_OUTPUT_BYTES + 4096;
    // Deterministic, bounded payload: N bytes of 'a'.
    let script = format!("head -c {want} /dev/zero | tr '\\0' 'a'");
    let req = ShellRequest::from_argv(["sh", "-c", &script]);

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("[output truncated"),
        "expected truncation marker, stdout len = {}",
        out.stdout.len()
    );
    // Strip the truncation marker before measuring the retained payload (the
    // marker text itself contains a couple of 'a's).
    let payload = out.stdout.split("\n[output truncated").next().unwrap_or("");
    let a_count = payload.matches('a').count();
    assert_eq!(
        a_count, MAX_STREAM_OUTPUT_BYTES,
        "retained payload should be exactly the byte cap (cap honored, no overflow)"
    );
}

// (5) cwd is respected: run `pwd` in a tempdir and assert it echoes that dir.
//     Uses the ctx cwd (request cwd = None) to exercise the ctx fallback.
#[tokio::test]
async fn ctx_cwd_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    let canon = std::fs::canonicalize(dir.path()).unwrap();
    let ctx = ctx_in(&canon);
    let req = ShellRequest::from_argv(["pwd"]);

    let out = run_direct(&req, &ctx).await.expect("pwd should run");
    assert_eq!(out.exit_code, 0);
    let reported = std::fs::canonicalize(out.stdout.trim()).unwrap();
    assert_eq!(reported, canon, "pwd should report the ctx tempdir");
}

// (5b) cwd from the request itself overrides the ctx cwd.
#[tokio::test]
async fn request_cwd_overrides_ctx_cwd() {
    let ctx_dir = tempfile::tempdir().unwrap();
    let req_dir = tempfile::tempdir().unwrap();
    let req_canon = std::fs::canonicalize(req_dir.path()).unwrap();
    let ctx = ctx_in(ctx_dir.path());

    let mut req = ShellRequest::from_argv(["pwd"]);
    req.cwd = Some(req_canon.clone());

    let out = run_direct(&req, &ctx).await.expect("pwd should run");
    let reported = std::fs::canonicalize(out.stdout.trim()).unwrap();
    assert_eq!(reported, req_canon, "request cwd should win over ctx cwd");
}

// Env vars from the request are passed to the child.
#[tokio::test]
async fn env_is_passed_to_child() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let mut env = HashMap::new();
    env.insert("WP_T_SHELL_VAR".to_string(), "marker-123".to_string());
    let mut req = ShellRequest::from_argv(["sh", "-c", "echo $WP_T_SHELL_VAR"]);
    req.env = env;

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert!(
        out.stdout.contains("marker-123"),
        "env var should be visible, got: {:?}",
        out.stdout
    );
}

// stderr is captured separately from stdout.
#[tokio::test]
async fn stderr_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["sh", "-c", "echo oops 1>&2; exit 3"]);

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert_eq!(out.exit_code, 3);
    assert!(out.stdout.is_empty(), "stdout should be empty");
    assert!(
        out.stderr.contains("oops"),
        "stderr should contain 'oops', got: {:?}",
        out.stderr
    );
}

// Empty command is rejected (mapped to ToolError::Other).
#[tokio::test]
async fn empty_command_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest {
        command: vec![],
        cwd: None,
        timeout_ms: None,
        env: HashMap::new(),
    };
    match run_direct(&req, &ctx).await {
        Err(ToolError::Other(_)) => {}
        other => panic!("expected Other for empty command, got {other:?}"),
    }
}

// Default timeout falls back to the codex/legacy 10s constant.
#[test]
fn default_timeout_constant() {
    assert_eq!(
        DEFAULT_SHELL_COMMAND_TIMEOUT_MS, 10_000,
        "default timeout must match codex/legacy 10s"
    );
}

// Denylist: a destructive `rm -rf /` is rejected at `run` (defense in depth).
#[tokio::test]
async fn destructive_command_rejected_at_run() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["rm", "-rf", "/"]);
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(msg)) => {
            assert!(
                msg.contains("denylist"),
                "rejection should mention denylist, got: {msg}"
            );
        }
        other => panic!("expected destructive command to be rejected, got {other:?}"),
    }
}

// Denylist: catches the `sh -c "rm -rf ~"` payload form too.
#[tokio::test]
async fn destructive_command_in_sh_c_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["sh", "-c", "rm -rf ~"]);
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(_)) => {}
        other => panic!("expected rejection for `sh -c rm -rf ~`, got {other:?}"),
    }
}

// Denylist: `sudo rm -rf /` (sudo-wrapped) is rejected.
#[tokio::test]
async fn sudo_destructive_command_rejected() {
    let req = ShellRequest::from_argv(["sudo", "rm", "-rf", "/"]);
    assert!(
        dangerous_command_rejection(&req.command).is_err(),
        "sudo-wrapped rm -rf / must be rejected"
    );
}

// Denylist: a benign scoped `rm -rf ./build` is NOT rejected (no false positive).
#[tokio::test]
async fn scoped_rm_is_not_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::create_dir(dir.path().join("build")).unwrap();
    let req = ShellRequest::from_argv(["rm", "-rf", "build"]);
    let out = run_direct(&req, &ctx)
        .await
        .expect("scoped rm should be allowed");
    assert_eq!(out.exit_code, 0);
    assert!(!dir.path().join("build").exists());
}

// The tool is not parallel-safe (serial / write-lock, matching codex shell).
#[test]
fn shell_is_not_parallel_safe() {
    let tool = ShellTool::new();
    let req = ShellRequest::from_argv(["echo", "x"]);
    assert!(!tool.parallel_safe(&req));
}

// Approval/sandbox accessors: a destructive command yields a Forbidden
// requirement; a benign one defers to the policy default (None).
#[test]
fn exec_approval_requirement_forbids_destructive_only() {
    use crate::tools::approval::ExecApprovalRequirement;
    let tool = ShellTool::new();
    let danger = ShellRequest::from_argv(["rm", "-rf", "/"]);
    let benign = ShellRequest::from_argv(["echo", "hi"]);

    assert!(
        matches!(
            tool.exec_approval_requirement(&danger),
            Some(ExecApprovalRequirement::Forbidden { .. })
        ),
        "destructive command must be forbidden"
    );
    assert!(
        tool.exec_approval_requirement(&benign).is_none(),
        "benign command defers to the policy default"
    );
    assert_eq!(
        tool.approval_keys(&benign).len(),
        1,
        "one approval key per shell call"
    );
    assert_eq!(
        tool.sandbox_permissions(&benign),
        SandboxPermissions::UseDefault
    );
}

// --- Orchestrator integration: drive the shell tool through the full seam. ---

fn turn_env(restricted: bool) -> TurnEnv {
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

#[tokio::test]
async fn orchestrated_echo_completes_under_none() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    // `Never` => no approval prompt for a benign command.
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = ShellTool::new();
    let req = ShellRequest::from_argv(["echo", "orchestrated"]);

    let result = orch
        .run(&tool, &req, &ctx, &turn_env(false), AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(result.output.stdout.contains("orchestrated"));
}

#[tokio::test]
async fn orchestrated_destructive_is_rejected_by_defense_in_depth() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    // Forbidden requirement => needs_initial_approval; AutoApprover approves,
    // but `run`'s defense-in-depth denylist still rejects the command.
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = ShellTool::new();
    let req = ShellRequest::from_argv(["rm", "-rf", "/"]);

    let err = orch
        .run(&tool, &req, &ctx, &turn_env(false), AskForApproval::Never)
        .await
        .expect_err("destructive command must not complete");
    assert!(
        matches!(err, ToolError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );
}
