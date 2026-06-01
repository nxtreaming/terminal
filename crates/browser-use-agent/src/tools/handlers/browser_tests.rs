//! Tests for the browser tool handler ([`BrowserTool`]).
//!
//! These NEVER touch the real `browser-use-browser` runtime (which requires
//! Bun + Chrome, an external CDP connection, and a local bridge port). Instead a
//! [`FakeBackend`] records the calls it receives and returns canned
//! `BrowserCommandOutput` / `BrowserScriptOutput` values, so the adapter's
//! translation and routing logic can be verified in isolation. No Bun, no
//! Chrome, no network.
//!
//! Tests cover: (1) a command request routes to the backend and maps output ->
//! ExecOutput; (2) script execute/observe/cancel route correctly; (3)
//! parallel_safe = false; (4) backend error -> ToolError; (5) an
//! orchestrator-driven run with the fake backend.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use browser_use_browser::{BrowserCommandOutput, BrowserScriptOutput};
use browser_use_store::Store;
use serde_json::json;

use super::browser::{BrowserAction, BrowserBackend, BrowserRequest, BrowserTool};
use crate::session::SharedStore;
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

/// Records which backend method was last invoked and with what arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum LastCall {
    #[default]
    None,
    Command(String),
    RunScript(String),
    StartScript(String),
    Observe(String),
    Cancel(String),
}

/// A configurable fake backend. By default every call succeeds; set `fail` to
/// make every call return an `anyhow` error.
#[derive(Default)]
struct FakeBackend {
    last: Mutex<LastCall>,
    last_session: Mutex<Option<String>>,
    last_paths: Mutex<Option<(PathBuf, PathBuf)>>,
    fail: bool,
}

impl FakeBackend {
    fn last(&self) -> LastCall {
        self.last.lock().unwrap().clone()
    }

    fn last_session(&self) -> Option<String> {
        self.last_session.lock().unwrap().clone()
    }

    fn last_paths(&self) -> Option<(PathBuf, PathBuf)> {
        self.last_paths.lock().unwrap().clone()
    }

    fn ok_command() -> BrowserCommandOutput {
        BrowserCommandOutput {
            content: json!({ "ok": true, "message": "navigated" }),
            events: vec![json!({ "type": "navigation" })],
        }
    }

    fn ok_script(status: Option<&str>, ok: bool) -> BrowserScriptOutput {
        BrowserScriptOutput {
            ok,
            status: status.map(|s| s.to_string()),
            run_id: Some("run-1".to_string()),
            text: "script-output".to_string(),
            ..Default::default()
        }
    }
}

impl BrowserBackend for FakeBackend {
    fn command(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<BrowserCommandOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last_paths.lock().unwrap() = Some((cwd.to_path_buf(), artifact_dir.to_path_buf()));
        *self.last.lock().unwrap() = LastCall::Command(command.to_string());
        if self.fail {
            anyhow::bail!("boom");
        }
        Ok(Self::ok_command())
    }

    fn run_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        _timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last_paths.lock().unwrap() = Some((cwd.to_path_buf(), artifact_dir.to_path_buf()));
        *self.last.lock().unwrap() = LastCall::RunScript(code.to_string());
        if self.fail {
            anyhow::bail!("boom");
        }
        // Foreground run completed successfully.
        Ok(Self::ok_script(None, true))
    }

    fn start_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        _timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last_paths.lock().unwrap() = Some((cwd.to_path_buf(), artifact_dir.to_path_buf()));
        *self.last.lock().unwrap() = LastCall::StartScript(code.to_string());
        if self.fail {
            anyhow::bail!("boom");
        }
        // A backgrounded start is still running.
        Ok(Self::ok_script(Some("running"), true))
    }

    fn observe_script(
        &self,
        session_id: &str,
        run_id: &str,
        _observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last.lock().unwrap() = LastCall::Observe(run_id.to_string());
        if self.fail {
            anyhow::bail!("unknown browser_script run_id {run_id:?}");
        }
        Ok(Self::ok_script(None, true))
    }

    fn cancel_script(&self, session_id: &str, run_id: &str) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last.lock().unwrap() = LastCall::Cancel(run_id.to_string());
        if self.fail {
            anyhow::bail!("unknown browser_script run_id {run_id:?}");
        }
        // Cancel reports a completed-but-not-ok run.
        Ok(Self::ok_script(None, false))
    }
}

fn tool_with(backend: Arc<FakeBackend>) -> BrowserTool {
    BrowserTool::with_backend(backend)
}

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
    ctx_with_call_id("call-browser")
}

fn ctx_with_call_id(call_id: &str) -> ToolCtx {
    let cwd = std::env::temp_dir();
    ToolCtx {
        call_id: call_id.to_string(),
        tool_name: "browser".to_string(),
        cwd: cwd.clone(),
        artifact_root: cwd.join("artifacts"),
    }
}

fn ctx_for_tool(tool_name: &str, call_id: &str) -> ToolCtx {
    let cwd = std::env::temp_dir();
    ToolCtx {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        cwd: cwd.clone(),
        artifact_root: cwd.join("artifacts"),
    }
}

fn shared_store() -> (tempfile::TempDir, SharedStore, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    let session = store
        .create_session(None, std::path::Path::new("/tmp"))
        .expect("create session")
        .id;
    (dir, Arc::new(Mutex::new(store)), session)
}

/// Run a request directly through the runtime with a `SandboxType::None`
/// attempt (no orchestrator).
async fn run_direct(tool: &BrowserTool, req: &BrowserRequest) -> Result<ExecOutput, ToolError> {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, &ctx()).await
}

async fn run_direct_with_ctx(
    tool: &BrowserTool,
    req: &BrowserRequest,
    ctx: &ToolCtx,
) -> Result<ExecOutput, ToolError> {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, ctx).await
}

// (1) A browser command request routes to the backend and maps output->ExecOutput.
#[tokio::test]
async fn command_routes_and_maps_output() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("sess-1", "go https://example.com");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(
        backend.last(),
        LastCall::Command("go https://example.com".to_string())
    );
    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("navigated"), "stdout: {}", out.stdout);
    assert!(
        out.stderr.contains("navigation"),
        "events should land on stderr: {}",
        out.stderr
    );
}

#[tokio::test]
async fn default_artifact_dir_comes_from_tool_ctx_artifact_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("cwd");
    let artifact_root = root.path().join("artifacts").join("session-1");
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));
    let ctx = ToolCtx {
        call_id: "call-browser".to_string(),
        tool_name: "browser".to_string(),
        cwd: cwd.clone(),
        artifact_root: artifact_root.clone(),
    };

    let req = BrowserRequest::execute("sess-1", "page_info()", false);
    run_direct_with_ctx(&tool, &req, &ctx).await.unwrap();

    assert_eq!(
        backend.last_paths(),
        Some((cwd, artifact_root)),
        "browser backend should receive separate cwd and artifact root"
    );
}

// (2) Script execute (foreground) routes to run_script.
#[tokio::test]
async fn execute_foreground_routes_to_run_script() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "click('#go')", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(
        backend.last(),
        LastCall::RunScript("click('#go')".to_string())
    );
    assert_eq!(out.stdout, "script-output");
    assert_eq!(out.exit_code, 0); // ok + not running
}

// (2) Script execute (background) routes to start_script and signals in-progress.
#[tokio::test]
async fn execute_background_routes_to_start_script_and_signals_in_progress() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "longRunning()", true);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(
        backend.last(),
        LastCall::StartScript("longRunning()".to_string())
    );
    // status == "running" -> sentinel exit code 2 (observe again).
    assert_eq!(out.exit_code, 2);
}

// (2) Observe routes to observe_script.
#[tokio::test]
async fn observe_routes_to_observe_script() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest {
        action: BrowserAction::Observe {
            run_id: "run-1".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: None,
    };
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(backend.last(), LastCall::Observe("run-1".to_string()));
    assert_eq!(out.exit_code, 0);
}

// (2) Cancel routes to cancel_script.
#[tokio::test]
async fn cancel_routes_to_cancel_script() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest {
        action: BrowserAction::Cancel {
            run_id: "run-1".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: None,
    };
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(backend.last(), LastCall::Cancel("run-1".to_string()));
    // cancel returns !ok in the fake -> exit code 1.
    assert_eq!(out.exit_code, 1);
}

// (3) parallel_safe = false, and approval/sandbox accessors are sane.
#[test]
fn browser_is_not_parallel_safe() {
    let tool = BrowserTool::with_backend(Arc::new(FakeBackend::default()));
    let req = BrowserRequest::command("sess-1", "screenshot");
    assert!(!tool.parallel_safe(&req));
    assert_eq!(tool.approval_keys(&req).len(), 1);
    assert!(tool.exec_approval_requirement(&req).is_none());
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
}

// (4) Error from backend -> ToolError::Other.
#[tokio::test]
async fn backend_error_maps_to_tool_error() {
    let backend = Arc::new(FakeBackend {
        fail: true,
        ..Default::default()
    });
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("sess-1", "go x");
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Other(_)), "got {err:?}");
}

#[tokio::test]
async fn observe_unknown_run_maps_to_error() {
    let backend = Arc::new(FakeBackend {
        fail: true,
        ..Default::default()
    });
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest {
        action: BrowserAction::Observe {
            run_id: "missing".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: None,
    };
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Other(_)), "got {err:?}");
}

// Validation: empty command/run_id are rejected before touching backend; an
// empty request session can fall back to the runtime context session id.
#[tokio::test]
async fn empty_command_rejected_without_calling_backend() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("sess-1", "   ");
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)), "got {err:?}");
    assert_eq!(backend.last(), LastCall::None);
}

#[tokio::test]
async fn empty_session_id_rejected() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("", "go x");
    let err = run_direct_with_ctx(&tool, &req, &ctx_with_call_id(""))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)), "got {err:?}");
    assert_eq!(backend.last(), LastCall::None);
}

#[tokio::test]
async fn empty_request_session_uses_context_session_id() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("", "go x");
    let out = run_direct_with_ctx(&tool, &req, &ctx_with_call_id("sess-from-ctx"))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(backend.last(), LastCall::Command("go x".to_string()));
}

#[tokio::test]
async fn configured_session_id_keeps_tool_call_id_for_persistence() {
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    let tool = tool_with(Arc::clone(&backend))
        .with_session_id("agent-session")
        .with_persistence(store.clone(), session.clone());

    let mut req = BrowserRequest::execute("", "extract()", false);
    req.session_id.clear();
    let out = run_direct_with_ctx(
        &tool,
        &req,
        &ctx_for_tool("browser_script", "model-call-123"),
    )
    .await
    .unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(backend.last_session().as_deref(), Some("agent-session"));
    let events = store.lock().unwrap().events_for_session(&session).unwrap();
    let output = events
        .iter()
        .find(|event| event.event_type == "tool.output")
        .expect("browser_script tool.output");
    assert_eq!(output.payload["name"], "browser_script");
    assert_eq!(output.payload["tool_call_id"], "model-call-123");
    assert_eq!(output.payload["text"], "script-output");
}

#[tokio::test]
async fn empty_run_id_rejected() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest {
        action: BrowserAction::Observe {
            run_id: "".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: None,
    };
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)), "got {err:?}");
    assert_eq!(backend.last(), LastCall::None);
}

// (5) Orchestrator-driven run with the fake backend (no Bun/Chrome).
#[tokio::test]
async fn orchestrated_command_runs_under_none() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let env = TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted: false,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    };

    let req = BrowserRequest::command("sess-1", "screenshot");
    let result = orch
        .run(&tool, &req, &ctx(), &env, AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(result.output.stdout.contains("navigated"));
    assert_eq!(backend.last(), LastCall::Command("screenshot".to_string()));
}
