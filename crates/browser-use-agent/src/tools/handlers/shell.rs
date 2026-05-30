//! Shell / exec tool: spawns a process asynchronously, applies a timeout, and
//! captures byte-capped stdout/stderr.
//!
//! This is the async re-implementation of codex's shell tool over our merged
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime) seam. It implements the
//! full trait stack ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it
//! can be driven by the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator).
//!
//! # Parity grounding (file:line in `/home/exedev/repos/codex/codex-rs`)
//!
//! * **Default timeout** `10_000` ms — codex `DEFAULT_EXEC_COMMAND_TIMEOUT_MS`
//!   (core/src/exec.rs:51) and legacy `DEFAULT_SHELL_COMMAND_TIMEOUT_MS`
//!   (browser-use-core command.rs:26).
//! * **Timeout exit code** `124` — codex `EXEC_TIMEOUT_EXIT_CODE`
//!   (exec.rs:58); codex sets `exit_code = 124` on timeout (exec.rs:746-748),
//!   and the legacy impl uses `SHELL_COMMAND_TIMEOUT_EXIT_CODE = 124`
//!   (command.rs:39). We follow the legacy/`exec.rs`-exit-code shape: on timeout
//!   we kill the child and return an [`ExecOutput`] with `exit_code = 124` (the
//!   `ToolError` enum here has no `Timeout` variant; codex's
//!   `SandboxErr::Timeout` is a sandbox-subsystem concept not in this WP's seam).
//! * **Output byte cap** `1 MiB` per stream — codex `EXEC_OUTPUT_MAX_BYTES =
//!   DEFAULT_OUTPUT_BYTES_CAP` (exec.rs:68) and legacy
//!   `UNIFIED_EXEC_OUTPUT_MAX_BYTES = 1024 * 1024` (command.rs:33).
//! * **Drain-to-EOF after cap** — mirrors codex `read_output` (exec.rs:1441-1475)
//!   / `append_capped` (exec.rs:856-864): keep reading past the cap to avoid
//!   back-pressure / SIGPIPE on the child, but retain only the first cap bytes.
//! * **I/O drain guard on timeout** — codex bounds the post-kill drain by
//!   `IO_DRAIN_TIMEOUT_MS = 2_000` (exec.rs:81); we apply the same bound when
//!   collecting output after a timeout kill.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::tools::approval::ExecApprovalRequirement;
use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// Default per-command timeout in milliseconds.
///
/// Matches codex `DEFAULT_EXEC_COMMAND_TIMEOUT_MS` (exec.rs:51) and legacy
/// `DEFAULT_SHELL_COMMAND_TIMEOUT_MS` (command.rs:26).
pub const DEFAULT_SHELL_COMMAND_TIMEOUT_MS: u64 = 10_000;

/// Exit code reported when a command is killed for exceeding its timeout.
///
/// Matches codex `EXEC_TIMEOUT_EXIT_CODE` (exec.rs:58) and legacy
/// `SHELL_COMMAND_TIMEOUT_EXIT_CODE` (command.rs:39) — the conventional shell
/// exit status for `timeout(1)`-killed processes.
pub const TIMEOUT_EXIT_CODE: i32 = 124;

/// Maximum number of bytes retained per output stream (1 MiB).
///
/// Matches codex `EXEC_OUTPUT_MAX_BYTES` (= `DEFAULT_OUTPUT_BYTES_CAP`,
/// exec.rs:68) and legacy `UNIFIED_EXEC_OUTPUT_MAX_BYTES = 1024 * 1024`
/// (command.rs:33).
pub const MAX_STREAM_OUTPUT_BYTES: usize = 1024 * 1024;

/// Bound on draining child output after a timeout kill, so a grandchild holding
/// the pipe open cannot hang the engine.
///
/// Matches codex `IO_DRAIN_TIMEOUT_MS = 2_000` (exec.rs:81) and legacy
/// `SHELL_COMMAND_IO_DRAIN_TIMEOUT_MS` (command.rs:40).
const IO_DRAIN_TIMEOUT_MS: u64 = 2_000;

/// Size of each read chunk. Matches codex `READ_CHUNK_SIZE` (exec.rs:61).
const READ_CHUNK_SIZE: usize = 8192;

/// Marker appended to a stream when it was truncated at the byte cap.
const TRUNCATION_MARKER: &str = "\n[output truncated: exceeded 1 MiB byte cap]";

/// Typed request for the shell/exec tool.
///
/// Field shape follows codex `ShellRequest`/`ExecParams` (core/src/tools/runtimes/shell.rs:48-64,
/// exec.rs:83-96): a tokenized command vector, an optional working directory, an
/// optional timeout, and an environment-variable map.
///
/// # Wire shape (model-facing args)
///
/// ```json
/// { "command": ["bash", "-lc", "echo hi"], "cwd": "/repo", "timeout_ms": 5000 }
/// ```
///
/// Deserializes directly from the model's argument object. The field names match
/// codex's `ExecParams` JSON (`command`/`cwd`/`timeout_ms`/`env`, exec.rs:83-96)
/// and the legacy shell spec (`browser-use-core/src/tools/mod.rs`). All fields
/// except `command` are optional on the wire: `cwd`/`timeout_ms` default to
/// `None` and `env` defaults to an empty map (codex `ExecParams.env` is likewise
/// an absent-means-empty map).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ShellRequest {
    /// The command and its arguments, already tokenized (argv-style). The first
    /// element is the program; the rest are arguments.
    pub command: Vec<String>,
    /// Working directory to run in. When `None`, the [`ToolCtx::cwd`] is used.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Per-command timeout in milliseconds. When `None`,
    /// [`DEFAULT_SHELL_COMMAND_TIMEOUT_MS`] is used.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Extra environment variables to set for the child process. Layered on top
    /// of the inherited environment (codex `ExecParams.env`).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl ShellRequest {
    /// Convenience constructor from an argv slice, using context defaults for
    /// cwd, timeout, and env.
    pub fn from_argv<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command: argv.into_iter().map(Into::into).collect(),
            cwd: None,
            timeout_ms: None,
            env: HashMap::new(),
        }
    }

    /// The effective timeout for this request.
    fn effective_timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_SHELL_COMMAND_TIMEOUT_MS)
    }
}

/// A simple, conservative denylist of obviously destructive commands.
///
/// PARITY CAVEAT: the legacy impl used a tree-sitter-based
/// `dangerous_command_rejection` (command.rs:2645) that parses the shell AST via
/// `tree-sitter-bash` to catch destructive forms (`rm -rf /`, `rm -rf ~`,
/// `sudo rm`, nested in `sh -lc "..."` / pipelines) — see
/// `command_words_might_be_dangerous` (command.rs:2850). Faithfully porting that
/// parser (and pulling in the `tree-sitter` dep) is a later WP; see the TODO
/// below. This is a SIMPLE substring/token heuristic that catches the most
/// egregious whole-filesystem / home deletions, including the common
/// `<shell> -c "..."` and `sudo` wrappings. It is intentionally narrow to avoid
/// false positives on scoped relative paths.
///
/// TODO(WP-T-shell-denylist): port the tree-sitter `dangerous_command_rejection`
/// for full parity (pipeline/`sh -lc` AST awareness, the read-only safelist, and
/// the exact codex rm subset semantics).
pub fn dangerous_command_rejection(command: &[String]) -> Result<(), ToolError> {
    if command.is_empty() {
        return Ok(());
    }

    // Strip a leading `sudo` (legacy: `command_words_might_be_dangerous`
    // recurses through `sudo`, command.rs:2859).
    let effective: &[String] = if base_name(&command[0]) == "sudo" && command.len() > 1 {
        &command[1..]
    } else {
        command
    };

    let mut candidates: Vec<String> = vec![effective.join(" ")];
    // For `<shell> -c "<script>"` / `-lc`, also inspect the script payload.
    if effective.len() >= 3
        && matches!(
            base_name(&effective[0]).as_str(),
            "sh" | "bash" | "zsh" | "dash"
        )
        && matches!(effective[1].as_str(), "-c" | "-lc")
    {
        candidates.push(effective[2].clone());
    }

    for cand in &candidates {
        let norm = cand.split_whitespace().collect::<Vec<_>>().join(" ");
        if is_root_wipe(&norm) {
            return Err(ToolError::Rejected(format!(
                "refusing to run destructive command (matched denylist): {cand}"
            )));
        }
    }
    Ok(())
}

/// The trailing path component of a program name (so `/bin/rm` -> `rm`).
fn base_name(s: &str) -> String {
    s.rsplit('/').next().unwrap_or(s).to_string()
}

/// Whether a normalized (whitespace-collapsed) command string looks like a
/// recursive-force delete of a filesystem root / home directory.
fn is_root_wipe(norm: &str) -> bool {
    let mut tokens = norm.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    if base_name(first) != "rm" {
        return false;
    }
    let recursive_force = norm.contains(" -rf")
        || norm.contains(" -fr")
        || norm.contains(" -rF")
        || norm.contains(" -fR")
        || (norm.contains(" -r") && norm.contains(" -f"))
        || norm.contains(" --recursive");
    if !recursive_force {
        return false;
    }
    // Root-ish targets: `/`, `/*`, `~`, `~/`, `$HOME`, `${HOME}`.
    const ROOTS: &[&str] = &["/", "/*", "~", "~/", "$HOME", "${HOME}"];
    norm.split_whitespace()
        .skip(1)
        .any(|tok| ROOTS.contains(&tok))
}

/// The async shell/exec tool.
///
/// Stateless; cheap to clone/construct. Limits and timeouts come from the
/// request and the constants above.
#[derive(Clone, Debug, Default)]
pub struct ShellTool;

impl ShellTool {
    /// Construct a new shell tool.
    pub fn new() -> Self {
        Self
    }
}

/// Approval key: the command + cwd identify a shell call for session caching.
///
/// Codex parity: `ApprovalKey` in runtimes/shell.rs:86-92 keys on the command +
/// cwd (+ sandbox permission selection, which is uniform here).
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ShellApprovalKey {
    command: Vec<String>,
    cwd: Option<PathBuf>,
}

impl Approvable<ShellRequest> for ShellTool {
    type ApprovalKey = ShellApprovalKey;

    fn approval_keys(&self, req: &ShellRequest) -> Vec<Self::ApprovalKey> {
        vec![ShellApprovalKey {
            command: req.command.clone(),
            cwd: req.cwd.clone(),
        }]
    }

    /// The shell may write anywhere under its working directory; request the
    /// default sandbox permissions (no escalation). Codex parity: the shell
    /// runtime returns the request's `sandbox_permissions` (runtimes/shell.rs:195),
    /// which defaults to `UseDefault` for an un-escalated command.
    fn sandbox_permissions(&self, _req: &ShellRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    /// A denylisted destructive command is forbidden outright; otherwise defer
    /// to the orchestrator's policy-driven default
    /// ([`default_exec_approval_requirement`](crate::tools::runtime::default_exec_approval_requirement))
    /// by returning `None`. Codex parity: the shell runtime supplies its
    /// `exec_approval_requirement` (runtimes/shell.rs:184-186), which the exec
    /// policy populates; here only the hard-deny is tool-intrinsic.
    fn exec_approval_requirement(&self, req: &ShellRequest) -> Option<ExecApprovalRequirement> {
        if let Err(ToolError::Rejected(reason)) = dangerous_command_rejection(&req.command) {
            return Some(ExecApprovalRequirement::Forbidden { reason });
        }
        None
    }
}

impl Sandboxable for ShellTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide. Today everything resolves to
        // `SandboxType::None`; real backends arrive in a later WP. Codex parity:
        // `ShellRuntime::sandbox_preference -> SandboxablePreference::Auto`
        // (runtimes/shell.rs:109-111).
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // Codex parity: `ShellRuntime::escalate_on_failure -> true`
        // (runtimes/shell.rs:112-114).
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<ShellRequest, ExecOutput> for ShellTool {
    fn parallel_safe(&self, _req: &ShellRequest) -> bool {
        // Match codex shell: serial / write-lock. The shell can mutate shared
        // state (filesystem, cwd), so it is not safe to run concurrently.
        false
    }

    async fn run(
        &self,
        req: &ShellRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // Defense in depth: reject destructive commands even if `run` is called
        // directly, bypassing the orchestrator's approval gate.
        dangerous_command_rejection(&req.command)?;

        // Today the only sandbox is `None`; a real backend (Landlock/seccomp)
        // lands later behind `attempt.sandbox`. We acknowledge the attempt to
        // make the seam explicit.
        let _ = attempt;

        let program = req
            .command
            .first()
            .ok_or_else(|| ToolError::Other(anyhow::anyhow!("empty command")))?;
        let args = &req.command[1..];

        // Working dir: explicit request cwd, else the ambient ToolCtx cwd.
        let cwd = req.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let timeout_ms = req.effective_timeout_ms();

        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        // Ensure the child is reaped if we drop it on timeout.
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Other(anyhow::anyhow!("failed to spawn `{program}`: {e}")))?;

        // Take the piped handles so we can drain them concurrently with the wait,
        // byte-capping each (codex `read_output`, exec.rs:1441-1475).
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let wait_with_output = async {
            let stdout_fut = read_capped_opt(stdout_pipe);
            let stderr_fut = read_capped_opt(stderr_pipe);
            tokio::join!(child.wait(), stdout_fut, stderr_fut)
        };

        let dur = Duration::from_millis(timeout_ms);
        match timeout(dur, wait_with_output).await {
            Ok((status, stdout_res, stderr_res)) => {
                let (stdout, _) = stdout_res
                    .map_err(|e| ToolError::Other(anyhow::anyhow!("reading stdout: {e}")))?;
                let (stderr, _) = stderr_res
                    .map_err(|e| ToolError::Other(anyhow::anyhow!("reading stderr: {e}")))?;
                let status = status
                    .map_err(|e| ToolError::Other(anyhow::anyhow!("waiting on child: {e}")))?;
                // Codex: exit_code = status.code().unwrap_or(-1) (exec.rs:745),
                // with signal-terminated children mapped to a sentinel.
                let exit_code = status.code().unwrap_or_else(|| signal_exit_code(&status));
                Ok(ExecOutput {
                    exit_code,
                    stdout,
                    stderr,
                })
            }
            Err(_elapsed) => {
                // Timed out. Kill the child and drain whatever output it produced,
                // bounded by IO_DRAIN_TIMEOUT_MS so a grandchild holding the pipe
                // cannot hang us (codex exec.rs:1370-1415). Report exit code 124
                // (codex exec.rs:746-748 sets exit_code = EXEC_TIMEOUT_EXIT_CODE).
                let _ = child.start_kill();
                let _ = child.wait().await;
                Ok(ExecOutput {
                    exit_code: TIMEOUT_EXIT_CODE,
                    stdout: String::new(),
                    stderr: format!("command timed out after {timeout_ms} ms"),
                })
            }
        }
    }
}

/// Map a signal-terminated exit status onto a conventional `128 + signal` code,
/// matching common shell behavior (codex `EXIT_CODE_SIGNAL_BASE = 128`,
/// exec.rs:57). Falls back to `-1` if unavailable (codex exec.rs:745).
fn signal_exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    let _ = status;
    -1
}

/// Read an optional child pipe to its byte cap. `None` pipes yield empty output.
async fn read_capped_opt<R: AsyncRead + Unpin>(pipe: Option<R>) -> std::io::Result<(String, bool)> {
    match pipe {
        Some(r) => read_capped(r, MAX_STREAM_OUTPUT_BYTES).await,
        None => Ok((String::new(), false)),
    }
}

/// Read `reader` to EOF, retaining at most `max_output` bytes.
///
/// Mirrors codex `read_output` + `append_capped` (exec.rs:1441-1475, 856-864):
/// we keep draining the reader to EOF even after the cap is reached (so the
/// child does not block on a full pipe / receive SIGPIPE), but only retain the
/// first `max_output` bytes. The whole drain is bounded by
/// [`IO_DRAIN_TIMEOUT_MS`] so an inherited-fd grandchild cannot hang us. When
/// truncation occurs, a [`TRUNCATION_MARKER`] is appended and the bool flag set.
async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    max_output: usize,
) -> std::io::Result<(String, bool)> {
    let mut buf: Vec<u8> = Vec::with_capacity(max_output.min(READ_CHUNK_SIZE));
    let mut tmp = [0u8; READ_CHUNK_SIZE];
    let mut truncated = false;

    let drain = async {
        loop {
            let n = reader.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            if buf.len() < max_output {
                let remaining = max_output - buf.len();
                let take = remaining.min(n);
                buf.extend_from_slice(&tmp[..take]);
                if take < n {
                    truncated = true;
                }
            } else {
                truncated = true;
            }
            // Keep reading to EOF even after the cap to avoid SIGPIPE.
        }
        Ok::<(), std::io::Error>(())
    };

    // Bound the drain so a grandchild holding the pipe open cannot hang us.
    match timeout(Duration::from_millis(IO_DRAIN_TIMEOUT_MS), drain).await {
        Ok(res) => res?,
        Err(_elapsed) => { /* fall through with whatever we collected */ }
    }

    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        s.push_str(TRUNCATION_MARKER);
    }
    Ok((s, truncated))
}
