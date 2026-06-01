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
//!   * the start/observe/cancel script path
//!     -> [`browser_use_browser::start_browser_script`] /
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

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use base64::{engine::general_purpose, Engine as _};
use browser_use_browser::{BrowserCommandOutput, BrowserScriptOutput};
use browser_use_llm::schema::ContentPart;
use serde_json::Value;

use crate::infra::{
    record_browser_command_response_events, record_browser_script_response_events_for_tool,
};
use crate::session::SharedStore;
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

/// Appended to `browser_script` stdout when the response carries image parts.
///
/// The dispatch layer strips this marker and re-wraps the JSON payload as typed
/// [`ContentPart`]s so provider protocols can send images to vision-capable
/// models while preserving a plain text fallback for logs/tests.
pub const BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX: &str = "\n__browser_script_content__:";

/// What the model wants the browser to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAction {
    /// Hidden `browser` command tool: a single command string evaluated by the
    /// browser runtime. Maps to [`browser_use_browser::run_browser_command`].
    Command {
        /// The raw command string (e.g. `go https://example.com`).
        command: String,
    },
    /// `browser_script`: start a script and either return its final result or a
    /// running handle, matching main's `browser_script action=start` behavior.
    Execute {
        /// The script body to run in the browser runtime.
        script: String,
        /// Compatibility field for older current-branch calls. Main ignores
        /// this concept; script execution always uses `start_browser_script`.
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
    /// Directory for run artifacts (screenshots, downloads). When `None`,
    /// [`ToolCtx::artifact_root`] is used.
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
/// { "code": "..." }
/// { "action": "start", "code": "..." }
/// { "action": "command", "command": "go https://example.com" }
/// { "action": "observe", "run_id": "r1" }
/// { "action": "cancel",  "run_id": "r1" }
/// ```
///
/// The variants mirror the existing [`BrowserAction`] cases and the legacy
/// model-facing browser paths (the hidden `browser <cmd>` command path and the
/// `browser_script` start/observe/cancel paths; see the module docs and legacy
/// `browser-use-core/src/tools/mod.rs`). `cwd` / `artifact_dir` are
/// carried-but-optional plumbing fields the router supplies; the per-action
/// payload fields (`command` / `script` / `run_id`) are validated by the `From`
/// adapter against the chosen `action`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct BrowserWireArgs {
    /// Which browser operation to perform.
    #[serde(default)]
    pub action: Option<BrowserActionKind>,
    /// Browser session id the action is bound to. The model normally omits this;
    /// production fills it from the current agent session through `ToolCtx`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Command string for the `command` action.
    #[serde(default)]
    pub command: Option<String>,
    /// Main-branch browser command field.
    #[serde(default)]
    pub cmd: Option<String>,
    /// Script body for the `start` action.
    #[serde(default)]
    pub script: Option<String>,
    /// Alias used by Codex-style tool schemas.
    #[serde(default)]
    pub code: Option<String>,
    /// Compatibility field for older current-branch calls.
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
    /// `browser_script` start path.
    #[serde(alias = "execute")]
    Start,
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
        let script = w.script.or(w.code);
        let command = w.cmd.or(w.command);
        let action_kind = w.action.unwrap_or_else(|| {
            if script.is_some() {
                BrowserActionKind::Start
            } else if command.is_some() {
                BrowserActionKind::Command
            } else if w.run_id.is_some() {
                BrowserActionKind::Observe
            } else {
                BrowserActionKind::Start
            }
        });
        let action = match action_kind {
            BrowserActionKind::Command => BrowserAction::Command {
                command: command.unwrap_or_default(),
            },
            BrowserActionKind::Start => BrowserAction::Execute {
                script: script.unwrap_or_default(),
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
            session_id: w.session_id.unwrap_or_default(),
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

/// Production backend: a thin delegation to `browser-use-browser`.
#[derive(Debug, Clone, Default)]
pub struct RealBackend {
    browser_mode: Option<String>,
}

impl RealBackend {
    pub fn with_browser_mode(browser_mode: Option<String>) -> Self {
        Self { browser_mode }
    }

    fn normalized_browser_mode(&self) -> Option<&str> {
        self.browser_mode
            .as_deref()
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .map(|mode| match mode {
                "cloud" | "browser-use-cloud" | "remote-cloud" => "cloud",
                "headless" | "headless-chromium" | "managed-headless" => "managed-headless",
                other => other,
            })
    }

    fn should_ensure_before_command(&self, command: &str) -> bool {
        if self.normalized_browser_mode().is_none() {
            return false;
        }
        let words = command_words(command);
        !matches!(
            words.as_slice(),
            ["browser", "remote", "start", ..]
                | ["remote", "start", ..]
                | ["browser", "remote", "stop", ..]
                | ["remote", "stop", ..]
        )
    }

    fn rewrite_command_for_mode(&self, command: &str) -> String {
        let words = command_words(command);
        if self.normalized_browser_mode() == Some("cloud")
            && matches!(
                words.as_slice(),
                ["browser", "local", ..]
                    | ["local", ..]
                    | ["browser", "connect", "local", ..]
                    | ["connect", "local", ..]
                    | ["browser", "connect", "managed", ..]
                    | ["connect", "managed", ..]
            )
        {
            return "browser status --json".to_string();
        }
        command.to_string()
    }

    fn ensure_configured_browser(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
    ) -> anyhow::Result<Vec<Value>> {
        let Some(mode) = self.normalized_browser_mode() else {
            return Ok(Vec::new());
        };
        let status = browser_use_browser::run_browser_command(
            session_id,
            cwd,
            artifact_dir,
            "browser status --json",
        )?;
        let mut events = status.events;
        let connected =
            status.content.get("connection").and_then(Value::as_str) == Some("connected");
        let current_mode = status.content.get("mode").and_then(Value::as_str);
        let desired_command = match mode {
            "cloud" => {
                if connected && current_mode == Some("remote-cloud") {
                    return Ok(events);
                }
                "browser remote start"
            }
            "managed-headless" => {
                if connected && current_mode == Some("managed") {
                    return Ok(events);
                }
                "browser connect managed --headless"
            }
            _ => return Ok(events),
        };
        let mut started = browser_use_browser::run_browser_command(
            session_id,
            cwd,
            artifact_dir,
            desired_command,
        )?;
        events.append(&mut started.events);
        Ok(events)
    }
}

fn command_words(command: &str) -> Vec<&str> {
    command
        .split_whitespace()
        .map(|word| word.trim_matches(|ch: char| ch == '"' || ch == '\''))
        .collect()
}

impl BrowserBackend for RealBackend {
    fn command(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<BrowserCommandOutput> {
        let mut events = if self.should_ensure_before_command(command) {
            self.ensure_configured_browser(session_id, cwd, artifact_dir)?
        } else {
            Vec::new()
        };
        let effective_command = self.rewrite_command_for_mode(command);
        let mut output = browser_use_browser::run_browser_command(
            session_id,
            cwd,
            artifact_dir,
            &effective_command,
        )?;
        if !events.is_empty() {
            events.append(&mut output.events);
            output.events = events;
        }
        Ok(output)
    }

    fn run_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        let mut events = self.ensure_configured_browser(session_id, cwd, artifact_dir)?;
        let mut output = browser_use_browser::run_browser_script(
            session_id,
            cwd,
            artifact_dir,
            code,
            timeout_secs,
        )?;
        if !events.is_empty() {
            events.append(&mut output.browser_events);
            output.browser_events = events;
        }
        Ok(output)
    }

    fn start_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        let mut events = self.ensure_configured_browser(session_id, cwd, artifact_dir)?;
        let mut output = browser_use_browser::start_browser_script(
            session_id,
            cwd,
            artifact_dir,
            code,
            timeout_secs,
        )?;
        if !events.is_empty() {
            events.append(&mut output.browser_events);
            output.browser_events = events;
        }
        Ok(output)
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

/// Map a [`BrowserScriptOutput`] into [`ExecOutput`], using the same
/// model-facing text contract as main's `browser_script` dispatcher.
fn map_script_output(out: BrowserScriptOutput) -> ExecOutput {
    let exit_code = if out.ok { 0 } else { 1 };
    let stderr = if out.ok {
        String::new()
    } else {
        out.error.clone().unwrap_or_default()
    };
    ExecOutput {
        exit_code,
        stdout: browser_script_stdout(&out),
        stderr,
    }
}

fn browser_script_stdout(response: &BrowserScriptOutput) -> String {
    let text = browser_script_tool_message_content(response);
    let (image_parts, warnings) = browser_script_image_parts(response);
    let text = append_browser_script_image_warnings(text, &warnings);
    let Some(payload) = browser_script_content_payload(&text, image_parts) else {
        return text;
    };
    format!("{text}{BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX}{payload}")
}

fn browser_script_content_payload(text: &str, image_parts: Vec<ContentPart>) -> Option<String> {
    if image_parts.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::text(text.to_string()));
    }
    parts.extend(image_parts);
    serde_json::to_string(&parts).ok()
}

fn browser_script_image_parts(response: &BrowserScriptOutput) -> (Vec<ContentPart>, Vec<String>) {
    let mut parts = Vec::new();
    let mut warnings = Vec::new();
    for image in &response.images {
        match browser_script_image_part(image) {
            Ok(Some(media)) => parts.push(media),
            Ok(None) => {}
            Err(warning) => warnings.push(warning),
        }
    }
    (parts, warnings)
}

fn append_browser_script_image_warnings(mut text: String, warnings: &[String]) -> String {
    for warning in warnings {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(warning);
    }
    text
}

fn browser_script_image_part(image: &Value) -> Result<Option<ContentPart>, String> {
    let Some(path) = image.get("path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let bytes = fs::read(path)
        .map_err(|error| format!("Warning: image artifact could not be read: {path} ({error})"))?;
    let mime_type = image
        .get("mime_type")
        .or_else(|| image.get("mime"))
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    if !mime_type.starts_with("image/") {
        return Ok(None);
    }
    Ok(Some(ContentPart::Media {
        mime_type: mime_type.to_string(),
        data: Some(general_purpose::STANDARD.encode(bytes)),
        url: None,
    }))
}

fn browser_script_tool_message_content(response: &BrowserScriptOutput) -> String {
    if response.status.as_deref() == Some("running") {
        return browser_script_running_message(response);
    }
    if response.status.as_deref() == Some("cancelled") {
        return browser_script_cancelled_message(response);
    }
    if response.ok {
        let mut parts = Vec::new();
        if !response.text.trim().is_empty() {
            parts.push(response.text.trim().to_string());
        }
        parts.extend(browser_script_structured_message_parts(response));
        if parts.is_empty() {
            "browser_script completed".to_string()
        } else {
            parts.join("\n")
        }
    } else {
        browser_script_failure_message(response)
    }
}

fn browser_script_running_message(response: &BrowserScriptOutput) -> String {
    let mut parts = Vec::new();
    if !response.text.trim().is_empty() {
        parts.push(response.text.trim().to_string());
    } else {
        parts.push("browser_script is still running.".to_string());
    }
    if let Some(run_id) = response.run_id.as_deref() {
        if !parts
            .iter()
            .any(|part| part.contains(&format!("run_id: {run_id}")))
        {
            parts.push(format!("run_id: {run_id}"));
        }
        parts.push(format!(
            "Next step: call browser_script with action=\"observe\", run_id=\"{run_id}\", and observe_timeout_ms={}.",
            response.next_observe_ms.unwrap_or(DEFAULT_OBSERVE_TIMEOUT_MS)
        ));
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_cancelled_message(response: &BrowserScriptOutput) -> String {
    let mut parts = Vec::new();
    if response.text.trim().is_empty() {
        parts.push("browser_script cancelled.".to_string());
    } else {
        parts.push(response.text.trim().to_string());
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_failure_message(response: &BrowserScriptOutput) -> String {
    let mut parts = vec!["browser_script failed.".to_string()];
    if let Some(diagnosis) = response.diagnosis.as_ref() {
        parts.push(diagnosis.summary.clone());
        parts.push(format!("What happened: {}", diagnosis.what_happened));
        parts.push(format!("Next step: {}", diagnosis.next_step));
    }
    if let Some(error) = response.error.as_deref() {
        let detail = browser_script_error_detail(error);
        if !detail.is_empty() {
            parts.push(format!("Details: {detail}"));
        }
    } else if response.diagnosis.is_none() {
        parts.push("Details: unknown browser_script error".to_string());
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_structured_message_parts(response: &BrowserScriptOutput) -> Vec<String> {
    let mut parts = Vec::new();
    if !response.outputs.is_empty() {
        parts.push(format!(
            "outputs: {}",
            Value::Array(response.outputs.clone())
        ));
    }
    if !response.summary.is_empty() {
        parts.push(format!(
            "summary: {}",
            Value::Array(response.summary.clone())
        ));
    }
    if !response.data.is_null() && response.data != serde_json::json!({}) {
        parts.push(format!("data: {}", response.data));
    }
    parts
}

fn browser_script_error_detail(error: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 500;
    let detail = error
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(error.trim());
    if detail.chars().count() <= MAX_DETAIL_CHARS {
        return detail.to_string();
    }
    let mut out = detail
        .chars()
        .take(MAX_DETAIL_CHARS.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

/// Browser tool handler.
///
/// Generic over the backend so production code uses [`RealBackend`] and tests
/// inject a fake. Construct with [`BrowserTool::new`] for the real backend or
/// [`BrowserTool::with_backend`] for a custom one.
#[derive(Clone)]
pub struct BrowserTool {
    backend: Arc<dyn BrowserBackend>,
    session_id_fallback: Option<String>,
    persistence: Option<BrowserPersistence>,
}

#[derive(Clone)]
struct BrowserPersistence {
    store: SharedStore,
    session_id: String,
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
            backend: Arc::new(RealBackend::default()),
            session_id_fallback: None,
            persistence: None,
        }
    }

    /// Construct a real browser tool with the run's configured browser mode.
    pub fn with_browser_mode(browser_mode: Option<String>) -> Self {
        Self {
            backend: Arc::new(RealBackend::with_browser_mode(browser_mode)),
            session_id_fallback: None,
            persistence: None,
        }
    }

    /// Construct a browser tool with a custom backend (used by tests).
    pub fn with_backend(backend: Arc<dyn BrowserBackend>) -> Self {
        Self {
            backend,
            session_id_fallback: None,
            persistence: None,
        }
    }

    /// Configure the browser tool with the live agent session id. The model can
    /// omit `session_id`; the runtime supplies it while keeping `ToolCtx.call_id`
    /// available for the actual model tool-call id.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id_fallback = Some(session_id.into());
        self
    }

    /// Configure durable event persistence for rich browser outputs.
    pub fn with_persistence(mut self, store: SharedStore, session_id: impl Into<String>) -> Self {
        self.persistence = Some(BrowserPersistence {
            store,
            session_id: session_id.into(),
        });
        self
    }

    fn fallback_session_id<'a>(&'a self, ctx: &'a ToolCtx) -> &'a str {
        self.session_id_fallback
            .as_deref()
            .unwrap_or_else(|| ctx.call_id.trim())
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

        let effective_session_id = if req.session_id.trim().is_empty() {
            self.fallback_session_id(ctx).trim()
        } else {
            req.session_id.trim()
        };

        // Validate the request before touching the backend.
        if effective_session_id.is_empty() {
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
        let session_id = effective_session_id.to_string();
        let cwd = req.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let artifact_dir = req
            .artifact_dir
            .clone()
            .unwrap_or_else(|| ctx.artifact_root.clone());
        let timeout_secs = req.effective_timeout_secs();
        let observe_ms = req.effective_observe_ms();
        let action = req.action.clone();
        let persistence = self.persistence.clone();
        let tool_call_id = ctx.call_id.clone();
        let tool_name = if ctx.tool_name.trim().is_empty() {
            match &action {
                BrowserAction::Command { .. } => "browser".to_string(),
                BrowserAction::Execute { .. }
                | BrowserAction::Observe { .. }
                | BrowserAction::Cancel { .. } => "browser_script".to_string(),
            }
        } else {
            ctx.tool_name.clone()
        };

        // The browser fns are synchronous and spawn external processes; run on a
        // blocking thread so we never stall the async runtime.
        let result = tokio::task::spawn_blocking(move || -> Result<ExecOutput, ToolError> {
            match action {
                BrowserAction::Command { command } => {
                    let out = backend
                        .command(&session_id, &cwd, &artifact_dir, &command)
                        .map_err(ToolError::Other)?;
                    if let Some(persistence) = &persistence {
                        if let Ok(store) = persistence.store.lock() {
                            let _ = record_browser_command_response_events(
                                &store,
                                &persistence.session_id,
                                &tool_name,
                                &tool_call_id,
                                &out,
                            );
                        }
                    }
                    Ok(map_command_output(out))
                }
                BrowserAction::Execute { script, .. } => {
                    let out = backend
                        .start_script(&session_id, &cwd, &artifact_dir, &script, timeout_secs)
                        .map_err(ToolError::Other)?;
                    if let Some(persistence) = &persistence {
                        if let Ok(store) = persistence.store.lock() {
                            let _ = record_browser_script_response_events_for_tool(
                                &store,
                                &persistence.session_id,
                                &tool_name,
                                &tool_call_id,
                                &out,
                            );
                        }
                    }
                    Ok(map_script_output(out))
                }
                BrowserAction::Observe { run_id } => {
                    let out = backend
                        .observe_script(&session_id, &run_id, observe_ms)
                        .map_err(ToolError::Other)?;
                    if let Some(persistence) = &persistence {
                        if let Ok(store) = persistence.store.lock() {
                            let _ = record_browser_script_response_events_for_tool(
                                &store,
                                &persistence.session_id,
                                &tool_name,
                                &tool_call_id,
                                &out,
                            );
                        }
                    }
                    Ok(map_script_output(out))
                }
                BrowserAction::Cancel { run_id } => {
                    let out = backend
                        .cancel_script(&session_id, &run_id)
                        .map_err(ToolError::Other)?;
                    if let Some(persistence) = &persistence {
                        if let Ok(store) = persistence.store.lock() {
                            let _ = record_browser_script_response_events_for_tool(
                                &store,
                                &persistence.session_id,
                                &tool_name,
                                &tool_call_id,
                                &out,
                            );
                        }
                    }
                    Ok(map_script_output(out))
                }
            }
        })
        .await
        .map_err(|e| ToolError::Other(anyhow::anyhow!("browser task panicked: {e}")))?;

        result
    }
}
