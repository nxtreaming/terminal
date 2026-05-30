//! The hook runtime: select matching hooks for an event, run their commands
//! through a [`CommandRunner`] seam, and fold the per-hook decisions into a
//! single [`HookOutcome`].
//!
//! Parity sources:
//! - codex `core/src/hook_runtime.rs`:
//!   - `HookRunResult` / `HookOutcome` / `HookOutcome::is_blocked`
//!     (`:48-78`).
//!   - `HookRuntime::run` fold + deny short-circuit + additional-context
//!     collection (`:96-132`).
//!   - decision derivation: parse stdout JSON; else exit code `2` => block with
//!     stderr as reason (`:190-207`).
//!   - the `CommandRunner` seam + `CommandOutput` + `ShellCommandRunner`
//!     production impl (`:308-393`).
//!   - `DEFAULT_HOOK_TIMEOUT_SECS = 60` (`:20`).
//! - codex `hook_runtime_tests.rs` constructs the runtime via
//!   `HookRuntime::with_runner(cfg, session_id, Arc<dyn CommandRunner>)`
//!   (`:127`, `:150`, ...): the runtime dispatches THROUGH the seam. codex's
//!   own committed `run_one` still spawns inline with a noted follow-up
//!   (`hook_runtime.rs:400-402`); we wire the runtime through the seam directly
//!   (the cleaner end-state the codex tests assume), so production and tests
//!   share one code path.
//!
//! SANCTIONED ADDITION: when running hooks for [`HookEvent::PermissionRequest`]
//! the runtime emits a `PermissionRequest` `PendingEvent` through an injected
//! [`EventSink`] (see `events/mod.rs`). codex has no permission/approval hook
//! event; this is the user-requested permission flow signal. It aligns with
//! `tools/approval.rs` `ExecApprovalRequirement`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::events::EventSink;
use crate::events::PendingEvent;

use super::config::HookCommand;
use super::config::HooksConfig;
use super::event::HookDecision;
use super::event::HookEvent;
use super::event::HookInput;

/// Default per-hook timeout when the config does not specify one.
///
/// Matches codex `DEFAULT_HOOK_TIMEOUT_SECS`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:20`).
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

/// The event type string emitted for the permission-request flow. Kept LOCAL
/// to this module (per the task's seam guidance) rather than appended to
/// `events/names.rs`; it sits alongside the existing `approval.requested`
/// (`events/names.rs:17`).
pub const PERMISSION_REQUEST_EVENT: &str = "hook.permission_request";

/// The output of a command run via [`CommandRunner`].
///
/// Mirrors codex `CommandOutput`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:322-332`).
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Process exit code, if it completed.
    pub exit_code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// True when the command timed out.
    pub timed_out: bool,
}

/// A trait abstracting how a hook command is executed, so tests can inject a
/// fake runner instead of spawning real processes.
///
/// Mirrors codex `CommandRunner`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:310-319`).
/// Production wiring is [`ShellCommandRunner`] (a real `/bin/sh -c` spawn);
/// tests inject a fake (see `hooks/tests.rs`).
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `command` with `stdin_json` on stdin, honoring `timeout`, returning
    /// the captured output.
    async fn run(&self, command: &str, stdin_json: &str, timeout: Duration) -> CommandOutput;
}

/// The production [`CommandRunner`] that spawns a real `/bin/sh -c` process.
///
/// Mirrors codex `ShellCommandRunner`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:335-388`):
/// pipe stdin/stdout/stderr, write the JSON input, wait with a hard timeout,
/// and map the result into [`CommandOutput`]. This is a real (not stub)
/// production implementation.
pub struct ShellCommandRunner;

#[async_trait::async_trait]
impl CommandRunner for ShellCommandRunner {
    async fn run(&self, command: &str, stdin_json: &str, timeout_dur: Duration) -> CommandOutput {
        use std::process::Stdio;

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return CommandOutput {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("failed to spawn hook: {e}"),
                    timed_out: false,
                };
            }
        };
        let mut child = child;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_json.as_bytes()).await;
            // Dropping `stdin` here closes the pipe so the child sees EOF.
        }
        let fut = child.wait_with_output();
        match timeout(timeout_dur, fut).await {
            Ok(Ok(o)) => CommandOutput {
                exit_code: o.status.code(),
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                timed_out: false,
            },
            Ok(Err(e)) => CommandOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("hook io error: {e}"),
                timed_out: false,
            },
            Err(_) => CommandOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
            },
        }
    }
}

/// Construct the default production runner.
///
/// Mirrors codex `default_runner`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:391-393`).
pub fn default_runner() -> Arc<dyn CommandRunner> {
    Arc::new(ShellCommandRunner)
}

/// The result of running a single hook command.
///
/// Mirrors codex `HookRunResult`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:49-61`).
#[derive(Debug, Clone)]
pub struct HookRunResult {
    /// The decision parsed from stdout / derived from exit code (if any).
    pub decision: Option<HookDecision>,
    /// The raw stdout captured from the hook.
    pub stdout: String,
    /// The raw stderr captured from the hook.
    pub stderr: String,
    /// The process exit code, if the process completed.
    pub exit_code: Option<i32>,
    /// True when the hook timed out.
    pub timed_out: bool,
}

/// Aggregated outcome after running all matching hooks for an event.
///
/// Mirrors codex `HookOutcome`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:64-78`).
#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    /// When set, the action is blocked with this reason.
    pub block_reason: Option<String>,
    /// Additional context to inject back into the model, in execution order.
    pub additional_context: Vec<String>,
    /// Per-hook raw results, in execution order.
    pub results: Vec<HookRunResult>,
}

impl HookOutcome {
    /// True when any hook blocked the action.
    ///
    /// Matches codex `HookOutcome::is_blocked`
    /// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:74-77`).
    pub fn is_blocked(&self) -> bool {
        self.block_reason.is_some()
    }
}

/// Runs configured hooks for lifecycle events.
///
/// Mirrors codex `HookRuntime`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:80-216`), but
/// always dispatches through the [`CommandRunner`] seam (the shape the codex
/// tests assume via `with_runner`).
#[derive(Clone)]
pub struct HookRuntime {
    config: Arc<HooksConfig>,
    session_id: Option<String>,
    runner: Arc<dyn CommandRunner>,
    /// Optional sink for the sanctioned `PermissionRequest` event flow.
    event_sink: Option<Arc<dyn EventSink>>,
}

impl HookRuntime {
    /// Create a runtime wired to the production [`ShellCommandRunner`].
    ///
    /// Mirrors codex `HookRuntime::new`
    /// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:89-94`),
    /// except the runner is explicit so tests can swap it.
    pub fn new(config: HooksConfig, session_id: Option<String>) -> Self {
        Self::with_runner(config, session_id, default_runner())
    }

    /// Create a runtime with an injected [`CommandRunner`] (the test seam).
    ///
    /// Signature matches the constructor codex's `hook_runtime_tests.rs`
    /// invokes (`HookRuntime::with_runner(cfg, session_id, runner)`).
    pub fn with_runner(
        config: HooksConfig,
        session_id: Option<String>,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            session_id,
            runner,
            event_sink: None,
        }
    }

    /// Attach an [`EventSink`] used for the sanctioned `PermissionRequest`
    /// event flow. Without a sink, [`HookEvent::PermissionRequest`] hooks still
    /// run; only the `PendingEvent` emission is skipped.
    pub fn with_event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// The session id this runtime was constructed with.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Run all hooks matching `event` + `subject`, folding their decisions.
    ///
    /// Folding rules track codex `HookRuntime::run`
    /// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:96-132`):
    /// - groups are visited in declared order; within a group hooks run in
    ///   declared order.
    /// - a hook with no matcher subject (`subject == None`) matches every
    ///   group; otherwise `HookMatcherGroup::matches(subject)` gates the group.
    /// - each hook's `additional_context` is collected in order.
    /// - the FIRST blocking decision short-circuits: its reason becomes
    ///   `block_reason`, that result is recorded, and remaining hooks are
    ///   skipped.
    /// - a timed-out hook is recorded but does NOT block (codex
    ///   `hook_runtime_tests.rs::timeout_is_handled`,
    ///   `/home/exedev/repos/codex/codex-rs/core/src/hook_runtime_tests.rs:207-234`).
    ///
    /// For [`HookEvent::PermissionRequest`] a `PermissionRequest` `PendingEvent`
    /// is emitted through the injected [`EventSink`] before hooks run (the
    /// sanctioned permission flow).
    pub async fn run(
        &self,
        event: HookEvent,
        subject: Option<&str>,
        input: HookInput,
    ) -> HookOutcome {
        if event == HookEvent::PermissionRequest {
            self.emit_permission_request(subject, &input);
        }

        let groups = self.config.groups_for(event);
        let mut outcome = HookOutcome::default();
        for group in groups {
            let matches = match subject {
                Some(s) => group.matches(s),
                None => true,
            };
            if !matches {
                continue;
            }
            for hook in &group.hooks {
                let result = self.run_one(hook, &input).await;
                if let Some(decision) = &result.decision {
                    if let Some(ctx) = &decision.additional_context {
                        outcome.additional_context.push(ctx.clone());
                    }
                    if decision.is_block() {
                        outcome.block_reason = decision
                            .reason
                            .clone()
                            .or_else(|| Some("hook blocked the action".to_string()));
                        outcome.results.push(result);
                        return outcome;
                    }
                }
                outcome.results.push(result);
            }
        }
        outcome
    }

    /// Run a single hook command through the [`CommandRunner`] seam and derive
    /// its decision.
    ///
    /// Decision derivation matches codex `run_one`
    /// (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime.rs:190-207`):
    /// parse non-empty trimmed stdout as [`HookDecision`] JSON; if that yields
    /// nothing and the exit code is `2`, synthesize a block decision with the
    /// trimmed stderr as the reason (Claude Code's "exit 2 = block"
    /// convention).
    async fn run_one(&self, hook: &HookCommand, input: &HookInput) -> HookRunResult {
        let timeout_secs = hook.timeout_secs().unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS);
        let stdin_json = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());

        let output = self
            .runner
            .run(
                hook.command_line(),
                &stdin_json,
                Duration::from_secs(timeout_secs),
            )
            .await;

        if output.timed_out {
            return HookRunResult {
                decision: None,
                stdout: output.stdout,
                stderr: output.stderr,
                exit_code: output.exit_code,
                timed_out: true,
            };
        }

        let parsed = if !output.stdout.trim().is_empty() {
            serde_json::from_str::<HookDecision>(output.stdout.trim()).ok()
        } else {
            None
        };
        let decision = match parsed {
            Some(d) => Some(d),
            None if output.exit_code == Some(2) => Some(HookDecision {
                r#continue: Some(false),
                reason: Some(output.stderr.trim().to_string()),
                additional_context: None,
            }),
            None => None,
        };

        HookRunResult {
            decision,
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
            timed_out: false,
        }
    }

    /// Emit the sanctioned `PermissionRequest` `PendingEvent` through the
    /// injected sink (no-op when no sink / no session id is configured).
    ///
    /// SANCTIONED ADDITION (user-requested permission flow): the payload mirrors
    /// the data a permission decision needs and aligns with
    /// `tools/approval.rs` `ExecApprovalRequirement::Required { reason }` — the
    /// `reason` is read from `input.extra["reason"]` when present.
    fn emit_permission_request(&self, subject: Option<&str>, input: &HookInput) {
        let Some(sink) = self.event_sink.as_ref() else {
            return;
        };
        let Some(session_id) = self.session_id.as_deref() else {
            return;
        };
        let reason = input
            .extra
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let payload = json!({
            "hookEventName": HookEvent::PermissionRequest.as_str(),
            "toolName": subject,
            "toolInput": input.tool_input,
            "reason": reason,
        });
        sink.emit(PendingEvent::new(
            session_id.to_string(),
            PERMISSION_REQUEST_EVENT,
            payload,
        ));
    }
}
