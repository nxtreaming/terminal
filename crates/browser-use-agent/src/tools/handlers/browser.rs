//! Browser tool handler.
//!
//! SANCTIONED DIVERGENCE: this is browser-use's product surface and has no
//! codex analog. The handler is a THIN adapter over the existing
//! `browser-use-browser` crate. It translates a typed [`BrowserRequest`] into
//! the appropriate `browser-use-browser` call and maps the returned
//! `BrowserCommandOutput` / `BrowserScriptOutput` into the seam's
//! [`ExecOutput`].
//!
//! ## What it wraps
//!
//! Two legacy model-facing paths are modeled here:
//!   * the hidden `browser <cmd-string>` command path
//!     -> [`browser_use_browser::run_browser_command`]
//!   * the execute/observe/cancel script path
//!     -> [`browser_use_browser::run_browser_script`] /
//!        [`browser_use_browser::start_browser_script`] /
//!        [`browser_use_browser::observe_browser_script`] /
//!        [`browser_use_browser::cancel_browser_script`]
//!
//! ## Testability without Bun/Chrome
//!
//! The real `browser-use-browser` functions spawn a Bun + Chrome toolchain
//! (external processes, a CDP websocket, a local bridge port) that is not
//! present in CI/test environments. To keep the adapter testable we put the
//! browser backend behind a small [`BrowserBackend`] trait. The production
//! implementation, [`RealBackend`], delegates 1:1 to `browser-use-browser`;
//! tests inject a fake backend instead and never touch Bun/Chrome/network.
//!
//! ## Concurrency
//!
//! The `browser-use-browser` functions are synchronous and spawn external
//! processes. To avoid blocking the async runtime, [`BrowserTool::run`] invokes
//! the backend on a blocking thread via [`tokio::task::spawn_blocking`].
//!
//! Browser actions are NOT parallel-safe: a single browser session/CDP
//! connection is shared and serialized, matching the legacy tool set where the
//! browser tool is excluded from the parallel set.

use std::path::PathBuf;
use std::sync::Arc;

use browser_use_browser::{BrowserCommandOutput, BrowserScriptOutput};

use crate::tools::approval::ExecApprovalRequirement;
use crate::tools::runtime::{Approvable, Sandboxable};
use crate::tools::runtime::{ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// Default per-script timeout (seconds) when a request omits one.
///
/// The `browser-use-browser` script fns take a `timeout_seconds`; we default to
/// a generous 120s so a single page interaction has room to complete.
pub const DEFAULT_BROWSER_SCRIPT_TIMEOUT_SECS: u64 = 120;

/// Default observe poll window (ms) for [`BrowserAction::Observe`].
///
/// Mirrors the legacy default observe window used by the browser_script runtime.
pub const DEFAULT_OBSERVE_TIMEOUT_MS: u64 = 1_000;

/// What the model wants the browser to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAction {
    /// Hidden `browser` command tool: a single command string evaluated by the
    /// browser runtime. Maps to [`browser_use_browser::run_browser_command`].
    Command {
        /// The raw command string (e.g. `go https://example.com`).
        command: String,
    },
    /// `browser_execute`: run a script. When `background` is false this blocks
    /// for the result ([`browser_use_browser::run_browser_script`]); when true
    /// it starts the run and returns a handle
    /// ([`browser_use_browser::start_browser_script`]).
    Execute {
        /// The script body to run in the browser runtime.
        script: String,
        /// Whether to start the run in the background (observe later).
        background: bool,
    },
    /// `observe`: poll an in-flight run.
    /// Maps to [`browser_use_browser::observe_browser_script`].
    Observe {
        /// Run identifier returned by a backgrounded `Execute`.
        run_id: String,
    },
    /// `cancel`: stop an in-flight run.
    /// Maps to [`browser_use_browser::cancel_browser_script`].
    Cancel {
        /// Run identifier returned by a backgrounded `Execute`.
        run_id: String,
    },
}

/// Request payload for the browser tool.
///
/// The browser-use-browser fns are session-scoped and need a working directory
/// plus an artifact directory; those identifiers are carried here so the adapter
/// stays thin (it forwards them unchanged).
///
/// # Deserialization (via [`BrowserWireArgs`])
///
/// The model's JSON arg object is FLAT (`action`/`session_id`/`script`/… — see
/// [`BrowserWireArgs`]), whereas this `Req` holds a tagged [`BrowserAction`] enum
/// and carried plumbing. So `BrowserRequest` deserializes THROUGH the flat wire
/// args: `#[serde(from = "BrowserWireArgs")]` runs the
/// [`From<BrowserWireArgs>`](BrowserRequest::from) adapter after deserializing the
/// model object. This makes `BrowserRequest: Deserialize`, so the tool registers
/// with the registry's plain `register` (the registry deserializes the model
/// object straight into `BrowserRequest`). Behavior is unchanged — the adapter
/// only reshapes the already-parsed fields.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(from = "BrowserWireArgs")]
pub struct BrowserRequest {
    /// The action to perform.
    pub action: BrowserAction,
    /// Browser session id the action is bound to.
    pub session_id: String,
    /// Working directory for the browser runtime. When `None`, the
    /// [`ToolCtx::cwd`] is used.
    pub cwd: Option<PathBuf>,
    /// Directory for run artifacts (screenshots, downloads). When `None`, the
    /// effective cwd is used.
    pub artifact_dir: Option<PathBuf>,
    /// Script timeout in seconds (script paths only). When `None`,
    /// [`DEFAULT_BROWSER_SCRIPT_TIMEOUT_SECS`].
    pub timeout_secs: Option<u64>,
    /// Observe poll window in milliseconds (observe path only). When `None`,
    /// [`DEFAULT_OBSERVE_TIMEOUT_MS`].
    pub observe_timeout_ms: Option<u64>,
}

impl BrowserRequest {
    /// Convenience constructor for the `browser <cmd>` command path.
    pub fn command(session_id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            action: BrowserAction::Command {
                command: command.into(),
            },
            session_id: session_id.into(),
            cwd: None,
            artifact_dir: None,
            timeout_secs: None,
            observe_timeout_ms: None,
        }
    }

    /// Convenience constructor for the script execute path.
    pub fn execute(
        session_id: impl Into<String>,
        script: impl Into<String>,
        background: bool,
    ) -> Self {
        Self {
            action: BrowserAction::Execute {
                script: script.into(),
                background,
            },
            session_id: session_id.into(),
            cwd: None,
            artifact_dir: None,
            timeout_secs: None,
            observe_timeout_ms: None,
        }
    }

    fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(DEFAULT_BROWSER_SCRIPT_TIMEOUT_SECS)
    }

    fn effective_observe_ms(&self) -> u64 {
        self.observe_timeout_ms
            .unwrap_or(DEFAULT_OBSERVE_TIMEOUT_MS)
    }
}

/// Model-facing wire arguments for the browser tool.
///
/// [`BrowserRequest`] is a PARSED form: its [`BrowserAction`] is an internally
/// tagged enum whose payload fields differ per variant, and the request carries
/// plumbing fields (`cwd`/`artifact_dir`) the model never sets. So the registry
/// cannot deserialize a `BrowserRequest` directly. Instead this flat
/// `BrowserWireArgs` matches the JSON the model actually emits and an
/// [`From<BrowserWireArgs>`](BrowserRequest::from) adapter parses it into the
/// typed request (the registry registers the tool over `BrowserWireArgs`).
///
/// # Wire shape (model-facing args)
///
/// ```json
/// { "action": "execute", "session_id": "s1", "script": "...", "background": false }
/// { "action": "command", "session_id": "s1", "command": "go https://example.com" }
/// { "action": "observe", "session_id": "s1", "run_id": "r1" }
/// { "action": "cancel",  "session_id": "s1", "run_id": "r1" }
/// ```
///
/// The variants mirror the existing [`BrowserAction`] cases and the legacy
/// model-facing browser paths (the hidden `browser <cmd>` command path and the
/// `browser_execute`/`observe`/`cancel` script paths; see the module docs and
/// legacy `browser-use-core/src/tools/mod.rs`). `cwd` / `artifact_dir` are
/// carried-but-optional plumbing fields the router supplies; the per-action
/// payload fields (`command` / `script` / `run_id`) are validated by the `From`
/// adapter against the chosen `action`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct BrowserWireArgs {
    /// Which browser operation to perform.
    pub action: BrowserActionKind,
    /// Browser session id the action is bound to.
    pub session_id: String,
    /// Command string for the `command` action.
    #[serde(default)]
    pub command: Option<String>,
    /// Script body for the `execute` action.
    #[serde(default)]
    pub script: Option<String>,
    /// Whether an `execute` runs in the background (observe later).
    #[serde(default)]
    pub background: bool,
    /// Run identifier for the `observe` / `cancel` actions.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Working directory for the browser runtime.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Directory for run artifacts.
    #[serde(default)]
    pub artifact_dir: Option<PathBuf>,
    /// Script timeout in seconds (script paths only).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Observe poll window in milliseconds (observe path only).
    #[serde(default)]
    pub observe_timeout_ms: Option<u64>,
}

/// The `action` discriminator of [`BrowserWireArgs`], mirroring the
/// [`BrowserAction`] variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserActionKind {
    /// Hidden `browser <cmd>` command path.
    Command,
    /// `browser_execute` script path.
    Execute,
    /// Poll an in-flight run.
    Observe,
    /// Cancel an in-flight run.
    Cancel,
}

impl From<BrowserWireArgs> for BrowserRequest {
    /// Parse the flat model wire args into the typed [`BrowserRequest`].
    ///
    /// A payload field missing for the chosen `action` defaults to an empty
    /// string; the tool's `run` validation then rejects it with the same
    /// "must not be empty" error it uses for an explicitly-empty value (so a
    /// malformed call surfaces a clean rejection rather than a deserialize
    /// failure).
    fn from(w: BrowserWireArgs) -> Self {
        let action = match w.action {
            BrowserActionKind::Command => BrowserAction::Command {
                command: w.command.unwrap_or_default(),
            },
            BrowserActionKind::Execute => BrowserAction::Execute {
                script: w.script.unwrap_or_default(),
                background: w.background,
            },
            BrowserActionKind::Observe => BrowserAction::Observe {
                run_id: w.run_id.unwrap_or_default(),
            },
            BrowserActionKind::Cancel => BrowserAction::Cancel {
                run_id: w.run_id.unwrap_or_default(),
            },
        };
        BrowserRequest {
            action,
            session_id: w.session_id,
            cwd: w.cwd,
            artifact_dir: w.artifact_dir,
            timeout_secs: w.timeout_secs,
            observe_timeout_ms: w.observe_timeout_ms,
        }
    }
}

/// The seam over `browser-use-browser`.
///
/// Implemented for real by [`RealBackend`] (delegates to the wrapped crate) and
/// by a fake in tests so the adapter can be exercised without Bun/Chrome.
///
/// All methods are synchronous and may spawn external processes; the adapter is
/// responsible for running them off the async runtime. Errors are
/// `anyhow::Error`, mirroring the wrapped crate.
pub trait BrowserBackend: Send + Sync {
    /// Run a one-shot browser command. Wraps `run_browser_command`.
    fn command(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<BrowserCommandOutput>;

    /// Run a script to completion. Wraps `run_browser_script`.
    fn run_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput>;

    /// Start a script in the background. Wraps `start_browser_script`.
    fn start_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput>;

    /// Observe an in-flight run. Wraps `observe_browser_script`.
    fn observe_script(
        &self,
        session_id: &str,
        run_id: &str,
        observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput>;

    /// Cancel an in-flight run. Wraps `cancel_browser_script`.
    fn cancel_script(&self, session_id: &str, run_id: &str) -> anyhow::Result<BrowserScriptOutput>;
}

/// Production backend: a 1:1 delegation to `browser-use-browser`.
///
/// Every method is a straight pass-through. The wrapped functions require Bun +
/// Chrome at runtime, so this backend is never exercised in tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealBackend;

impl BrowserBackend for RealBackend {
    fn command(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<BrowserCommandOutput> {
        browser_use_browser::run_browser_command(session_id, cwd, artifact_dir, command)
    }

    fn run_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        browser_use_browser::run_browser_script(session_id, cwd, artifact_dir, code, timeout_secs)
    }

    fn start_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        browser_use_browser::start_browser_script(session_id, cwd, artifact_dir, code, timeout_secs)
    }

    fn observe_script(
        &self,
        session_id: &str,
        run_id: &str,
        observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        browser_use_browser::observe_browser_script(session_id, run_id, observe_timeout_ms)
    }

    fn cancel_script(&self, session_id: &str, run_id: &str) -> anyhow::Result<BrowserScriptOutput> {
        browser_use_browser::cancel_browser_script(session_id, run_id)
    }
}

/// Map a one-shot [`BrowserCommandOutput`] into [`ExecOutput`].
///
/// The command runtime returns a structured `content` JSON plus an `events`
/// list. We serialize `content` onto stdout (the model-facing payload) and the
/// events list onto stderr, with `exit_code = 0` (a failed command surfaces its
/// failure inside `content`; the wrapped fn errors are handled separately).
fn map_command_output(out: BrowserCommandOutput) -> ExecOutput {
    let stdout = match serde_json::to_string(&out.content) {
        Ok(s) => s,
        Err(e) => format!("<unserializable browser content: {e}>"),
    };
    let stderr = if out.events.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&out.events).unwrap_or_default()
    };
    ExecOutput {
        exit_code: 0,
        stdout,
        stderr,
    }
}

/// Map a [`BrowserScriptOutput`] into [`ExecOutput`].
///
/// `text` is the accumulated model-facing output (stdout). A script `error`
/// goes to stderr. Exit code mirrors the run's `ok` flag, with a sentinel `2`
/// when the run is still `running` (the model should observe again).
fn map_script_output(out: BrowserScriptOutput) -> ExecOutput {
    let still_running = out.status.as_deref() == Some("running");
    let exit_code = if still_running {
        2
    } else if out.ok {
        0
    } else {
        1
    };
    let stderr = out.error.unwrap_or_default();
    ExecOutput {
        exit_code,
        stdout: out.text,
        stderr,
    }
}

/// Browser tool handler.
///
/// Generic over the backend so production code uses [`RealBackend`] and tests
/// inject a fake. Construct with [`BrowserTool::new`] for the real backend or
/// [`BrowserTool::with_backend`] for a custom one.
#[derive(Clone)]
pub struct BrowserTool {
    backend: Arc<dyn BrowserBackend>,
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BrowserTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserTool").finish_non_exhaustive()
    }
}

impl BrowserTool {
    /// Construct a browser tool backed by the real `browser-use-browser`
    /// runtime.
    pub fn new() -> Self {
        Self {
            backend: Arc::new(RealBackend),
        }
    }

    /// Construct a browser tool with a custom backend (used by tests).
    pub fn with_backend(backend: Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

/// Approval key: the session + action identify a browser call for session
/// caching, mirroring the shape the other handlers use. The browser tool needs
/// no approval by default (see [`Approvable::exec_approval_requirement`]).
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct BrowserApprovalKey {
    session_id: String,
    action: String,
}

impl Approvable<BrowserRequest> for BrowserTool {
    type ApprovalKey = BrowserApprovalKey;

    fn approval_keys(&self, req: &BrowserRequest) -> Vec<Self::ApprovalKey> {
        let action = match &req.action {
            BrowserAction::Command { .. } => "command",
            BrowserAction::Execute { .. } => "execute",
            BrowserAction::Observe { .. } => "observe",
            BrowserAction::Cancel { .. } => "cancel",
        };
        vec![BrowserApprovalKey {
            session_id: req.session_id.clone(),
            action: action.to_string(),
        }]
    }

    /// The browser runtime manages its own session; request the default sandbox
    /// permissions (no escalation).
    fn sandbox_permissions(&self, _req: &BrowserRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` left at its trait default (`None`): the
    // browser tool requires no approval by default, mirroring the legacy
    // browser_* tools. The orchestrator applies the policy default, which yields
    // `Skip` under any non-prompting policy.
    fn exec_approval_requirement(&self, _req: &BrowserRequest) -> Option<ExecApprovalRequirement> {
        None
    }
}

impl Sandboxable for BrowserTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // The browser runtime spawns its own external processes and manages its
        // own isolation; let the provider decide (today everything resolves to
        // `SandboxType::None`). `Auto` keeps the seam uniform with the other
        // tools.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // A browser failure is not a sandbox denial we can usefully retry
        // unsandboxed; keep it uniform with the other tools.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<BrowserRequest, ExecOutput> for BrowserTool {
    fn parallel_safe(&self, _req: &BrowserRequest) -> bool {
        // Browser actions share a single session/CDP connection and must run
        // serially. This matches the legacy tool set, which excludes the browser
        // tool from the parallel set.
        false
    }

    async fn run(
        &self,
        req: &BrowserRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox backend is exercised here (the browser runtime spawns its
        // own processes); acknowledge the attempt to make the seam explicit.
        let _ = attempt;

        // Validate the request before touching the backend.
        if req.session_id.trim().is_empty() {
            return Err(ToolError::Rejected(
                "browser session_id must not be empty".to_string(),
            ));
        }
        match &req.action {
            BrowserAction::Command { command } if command.trim().is_empty() => {
                return Err(ToolError::Rejected(
                    "browser command must not be empty".to_string(),
                ));
            }
            BrowserAction::Execute { script, .. } if script.trim().is_empty() => {
                return Err(ToolError::Rejected(
                    "browser script must not be empty".to_string(),
                ));
            }
            BrowserAction::Observe { run_id } | BrowserAction::Cancel { run_id }
                if run_id.trim().is_empty() =>
            {
                return Err(ToolError::Rejected(
                    "browser run_id must not be empty".to_string(),
                ));
            }
            _ => {}
        }

        let backend = Arc::clone(&self.backend);
        let session_id = req.session_id.clone();
        let cwd = req.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let artifact_dir = req.artifact_dir.clone().unwrap_or_else(|| cwd.clone());
        let timeout_secs = req.effective_timeout_secs();
        let observe_ms = req.effective_observe_ms();
        let action = req.action.clone();

        // The browser fns are synchronous and spawn external processes; run on a
        // blocking thread so we never stall the async runtime.
        let result = tokio::task::spawn_blocking(move || -> Result<ExecOutput, ToolError> {
            match action {
                BrowserAction::Command { command } => {
                    let out = backend
                        .command(&session_id, &cwd, &artifact_dir, &command)
                        .map_err(ToolError::Other)?;
                    Ok(map_command_output(out))
                }
                BrowserAction::Execute { script, background } => {
                    let out = if background {
                        backend.start_script(
                            &session_id,
                            &cwd,
                            &artifact_dir,
                            &script,
                            timeout_secs,
                        )
                    } else {
                        backend.run_script(&session_id, &cwd, &artifact_dir, &script, timeout_secs)
                    }
                    .map_err(ToolError::Other)?;
                    Ok(map_script_output(out))
                }
                BrowserAction::Observe { run_id } => {
                    let out = backend
                        .observe_script(&session_id, &run_id, observe_ms)
                        .map_err(ToolError::Other)?;
                    Ok(map_script_output(out))
                }
                BrowserAction::Cancel { run_id } => {
                    let out = backend
                        .cancel_script(&session_id, &run_id)
                        .map_err(ToolError::Other)?;
                    Ok(map_script_output(out))
                }
            }
        })
        .await
        .map_err(|e| ToolError::Other(anyhow::anyhow!("browser task panicked: {e}")))?;

        result
    }
}
