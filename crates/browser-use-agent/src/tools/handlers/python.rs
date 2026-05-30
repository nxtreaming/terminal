//! Python tool handler: thin adapter over the `browser-use-python-worker` crate.
//!
//! SANCTIONED DIVERGENCE: codex has no python tool. This is browser-use's own
//! product surface — the model authors a Python snippet, we run it in a
//! persistent worker subprocess, and map the structured result into
//! [`ExecOutput`]. The handler is a THIN adapter over the existing
//! `browser-use-python-worker` crate, structured exactly like
//! [`super::browser`] (a backend trait + a real delegate + a fake for tests).
//!
//! ## What it wraps
//!
//! [`browser_use_python_worker::PythonWorker::run`] (lib.rs:259): submit a code
//! snippet to the persistent worker and get back a
//! [`browser_use_python_worker::RunPythonResponse`] (lib.rs:46). The
//! `RealBackend` constructs the worker via
//! `PythonWorker::start_with_browser_mode_and_env` (lib.rs:84).
//!
//! ## Parity with the legacy dispatch
//!
//! Legacy `dispatch_python_tool` (browser-use-core `src/lib.rs:20204`, called at
//! lib.rs:11661) streams worker events, records artifacts, and returns
//! text + image content parts. For THIS thin WP we do a synchronous
//! `run -> ExecOutput` mapping mirroring the legacy request/response shape; the
//! richer `outputs` / `artifacts` / `images` / `browser_events` are surfaced as
//! text on stdout/stderr. Event STREAMING and artifact RECORDING are deferred
//! (see the TODO on [`map_response`]).
//!
//! ## Testability without Python / Bun / network
//!
//! The real worker spawns an external Python (uv/python3) process that is not
//! present in CI/test environments. So, exactly like `browser.rs`, the worker
//! lives behind a [`PythonBackend`] trait: [`RealBackend`] delegates to
//! `browser-use-python-worker`; tests inject a fake backend and never touch
//! Python/Bun/network.
//!
//! ## Concurrency
//!
//! `PythonWorker::run` is synchronous, takes `&mut self`, and performs blocking
//! I/O against the child process. The `RealBackend` therefore guards the worker
//! with a `std::sync::Mutex` and runs the call on a blocking thread via
//! [`tokio::task::spawn_blocking`], like `browser.rs`'s session work.
//!
//! ## Concurrency policy
//!
//! `parallel_safe = false`: a single worker process holds shared interpreter
//! state, so snippets must run serially. This matches the legacy python tool,
//! which is a hidden/serial handler.

use std::path::Path;
use std::sync::{Arc, Mutex};

use browser_use_python_worker::{PythonWorker, RunPythonResponse};
use serde_json::Value;

use crate::tools::approval::ExecApprovalRequirement;
use crate::tools::runtime::{Approvable, Sandboxable};
use crate::tools::runtime::{ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// Default session id used when a request does not carry one.
///
/// The worker keeps a persistent per-session namespace; a stable default keeps
/// snippets in one interpreter namespace across a turn.
pub const DEFAULT_PYTHON_SESSION_ID: &str = "default";

/// Request payload for the python tool.
///
/// Mirrors the model-facing python tool arguments and the worker's request
/// shape (`code` plus an optional timeout). `session_id` / `cwd` / `artifact_dir`
/// are carried so the adapter stays thin (it forwards them unchanged); each
/// falls back to a sensible default when `None`.
///
/// # Wire shape (model-facing args)
///
/// ```json
/// { "code": "print(1+1)", "timeout_secs": 30 }
/// ```
///
/// Deserializes directly from the model's argument object. The `code` field name
/// matches the legacy python tool arg (`browser-use-core/src/lib.rs`
/// `dispatch_python_tool`); only `code` is required. `session_id` / `cwd` /
/// `artifact_dir` / `timeout_secs` are carried-but-optional plumbing fields
/// (the router/orchestrator supplies cwd/artifact_dir), each `#[serde(default)]`
/// so deserialization of `{ "code": ... }` succeeds.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct PythonRequest {
    /// The Python source code to execute in the worker.
    pub code: String,
    /// Worker session id (persistent namespace). When `None`,
    /// [`DEFAULT_PYTHON_SESSION_ID`].
    #[serde(default)]
    pub session_id: Option<String>,
    /// Working directory for the snippet. When `None`, the [`ToolCtx::cwd`].
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
    /// Directory for run artifacts. When `None`, the effective cwd.
    #[serde(default)]
    pub artifact_dir: Option<std::path::PathBuf>,
    /// Optional timeout in seconds for this snippet.
    #[serde(default)]
    pub timeout_secs: Option<f64>,
}

impl PythonRequest {
    /// Convenience constructor from a code string, defaulting everything else.
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            session_id: None,
            cwd: None,
            artifact_dir: None,
            timeout_secs: None,
        }
    }

    fn effective_session_id(&self) -> &str {
        self.session_id
            .as_deref()
            .unwrap_or(DEFAULT_PYTHON_SESSION_ID)
    }
}

/// The seam over `browser-use-python-worker`.
///
/// Implemented for real by [`RealBackend`] (delegates to the wrapped crate) and
/// by a fake in tests so the adapter can be exercised without Python/Bun.
///
/// The method submits a snippet and returns the worker's structured
/// [`RunPythonResponse`]; it is synchronous and may spawn / drive an external
/// process, so the adapter runs it off the async runtime. Errors are
/// `anyhow::Error`, mirroring the wrapped crate.
pub trait PythonBackend: Send + Sync {
    /// Run a Python snippet. Wraps [`PythonWorker::run`].
    fn run(
        &self,
        session_id: &str,
        cwd: &Path,
        artifact_dir: &Path,
        code: &str,
        timeout_secs: Option<f64>,
    ) -> anyhow::Result<RunPythonResponse>;
}

/// Production backend backed by a started [`PythonWorker`].
///
/// The worker is guarded by a `Mutex` because [`PythonWorker::run`] takes
/// `&mut self`. This backend requires a real Python toolchain at runtime, so it
/// is never exercised in tests.
pub struct RealBackend {
    worker: Mutex<PythonWorker>,
}

impl RealBackend {
    /// Wrap an already-started worker.
    pub fn new(worker: PythonWorker) -> Self {
        Self {
            worker: Mutex::new(worker),
        }
    }

    /// Start a worker (spawning an external Python process) and wrap it.
    ///
    /// `browser_mode` and `extra_env` are forwarded verbatim to
    /// `PythonWorker::start_with_browser_mode_and_env` (lib.rs:84).
    pub fn start(
        browser_mode: Option<&str>,
        extra_env: &[(String, String)],
    ) -> anyhow::Result<Self> {
        let worker = PythonWorker::start_with_browser_mode_and_env(
            browser_mode,
            extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        )?;
        Ok(Self::new(worker))
    }
}

impl PythonBackend for RealBackend {
    fn run(
        &self,
        session_id: &str,
        cwd: &Path,
        artifact_dir: &Path,
        code: &str,
        timeout_secs: Option<f64>,
    ) -> anyhow::Result<RunPythonResponse> {
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| anyhow::anyhow!("python worker mutex poisoned"))?;
        // TODO(streaming): the legacy dispatch uses `run_with_events_and_timeout`
        // and streams `PythonWorkerEvent`s; for this thin WP we collapse to the
        // non-streaming `run_with_timeout`, which yields the same final
        // `RunPythonResponse`.
        worker.run_with_timeout(session_id, cwd, artifact_dir, code, timeout_secs)
    }
}

/// Join the `text` field of structured worker entries (outputs / events) onto a
/// single newline-separated string, falling back to the raw JSON for entries
/// that are not `{ "text": ... }` shaped.
fn join_text_entries(entries: &[Value]) -> String {
    entries
        .iter()
        .map(|entry| match entry.get("text").and_then(Value::as_str) {
            Some(text) => text.to_string(),
            None => entry.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Map a [`RunPythonResponse`] into [`ExecOutput`].
///
/// Mapping (mirrors the legacy result shape):
/// - `stdout`: the snippet's `text` (the model-facing output), with any
///   expression `outputs` appended.
/// - `stderr`: the snippet's `error` message (if any).
/// - `exit_code`: `0` when `ok`, else `1`.
///
/// TODO(streaming/artifacts): the legacy `dispatch_python_tool`
/// (browser-use-core lib.rs:20204) streams `browser_events` and records
/// `artifacts` / `images` as image/text content parts. That richer surface is
/// deferred to a later WP; here `artifacts` / `images` / `browser_events` are
/// summarized as text on stderr so nothing is silently dropped.
pub fn map_response(resp: RunPythonResponse) -> ExecOutput {
    let mut stdout = resp.text;
    if !resp.outputs.is_empty() {
        let joined = join_text_entries(&resp.outputs);
        if !joined.is_empty() {
            if !stdout.is_empty() && !stdout.ends_with('\n') {
                stdout.push('\n');
            }
            stdout.push_str(&joined);
        }
    }

    let mut stderr = String::new();
    if let Some(err) = resp.error {
        stderr.push_str(&err);
    }
    // Surface deferred-surface counts so artifacts/images/browser_events are not
    // silently dropped (full recording is a later WP — see map_response docs).
    let mut notes = Vec::new();
    if !resp.artifacts.is_empty() {
        notes.push(format!("{} artifact(s)", resp.artifacts.len()));
    }
    if !resp.images.is_empty() {
        notes.push(format!("{} image(s)", resp.images.len()));
    }
    if !resp.browser_events.is_empty() {
        notes.push(format!("{} browser event(s)", resp.browser_events.len()));
    }
    if !notes.is_empty() {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "[python: {} (recording deferred)]",
            notes.join(", ")
        ));
    }

    ExecOutput {
        exit_code: if resp.ok { 0 } else { 1 },
        stdout,
        stderr,
    }
}

/// Python tool handler.
///
/// Generic over the backend so production code uses [`RealBackend`] and tests
/// inject a fake. Construct with [`PythonTool::with_backend`].
#[derive(Clone)]
pub struct PythonTool {
    backend: Arc<dyn PythonBackend>,
}

impl std::fmt::Debug for PythonTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonTool").finish_non_exhaustive()
    }
}

impl PythonTool {
    /// Construct a python tool with the given backend.
    pub fn with_backend(backend: Arc<dyn PythonBackend>) -> Self {
        Self { backend }
    }
}

/// Approval key: the session identifies a python call for session caching,
/// mirroring the shape the other handlers use. The python tool needs no
/// approval by default (see [`Approvable::exec_approval_requirement`]).
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct PythonApprovalKey {
    session_id: String,
}

impl Approvable<PythonRequest> for PythonTool {
    type ApprovalKey = PythonApprovalKey;

    fn approval_keys(&self, req: &PythonRequest) -> Vec<Self::ApprovalKey> {
        vec![PythonApprovalKey {
            session_id: req.effective_session_id().to_string(),
        }]
    }

    /// The worker manages its own process; request the default sandbox
    /// permissions (no escalation).
    fn sandbox_permissions(&self, _req: &PythonRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` left at its trait default (`None`): the python
    // tool requires no approval by default, mirroring the legacy hidden handler.
    fn exec_approval_requirement(&self, _req: &PythonRequest) -> Option<ExecApprovalRequirement> {
        None
    }
}

impl Sandboxable for PythonTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // The worker spawns its own external process and manages its own
        // isolation; let the provider decide (today everything resolves to
        // `SandboxType::None`). `Auto` keeps the seam uniform with the other
        // tools.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // A python failure is not a sandbox denial we can usefully retry
        // unsandboxed; keep it uniform with the other tools.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<PythonRequest, ExecOutput> for PythonTool {
    fn parallel_safe(&self, _req: &PythonRequest) -> bool {
        // The worker holds shared interpreter state; snippets must run serially.
        // Matches the legacy hidden/serial python handler.
        false
    }

    async fn run(
        &self,
        req: &PythonRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox backend is exercised here (the worker spawns its own
        // process); acknowledge the attempt to make the seam explicit.
        let _ = attempt;

        if req.code.trim().is_empty() {
            return Err(ToolError::Rejected(
                "python code must not be empty".to_string(),
            ));
        }

        let backend = Arc::clone(&self.backend);
        let session_id = req.effective_session_id().to_string();
        let cwd = req.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let artifact_dir = req.artifact_dir.clone().unwrap_or_else(|| cwd.clone());
        let code = req.code.clone();
        let timeout_secs = req.timeout_secs;

        // The worker call is synchronous, blocking I/O against an external
        // process; run on a blocking thread so we never stall the async runtime.
        let resp = tokio::task::spawn_blocking(move || {
            backend.run(&session_id, &cwd, &artifact_dir, &code, timeout_secs)
        })
        .await
        .map_err(|e| ToolError::Other(anyhow::anyhow!("python task panicked: {e}")))?
        .map_err(ToolError::Other)?;

        Ok(map_response(resp))
    }
}
