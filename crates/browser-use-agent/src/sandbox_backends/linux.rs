//! Real Linux sandbox backend via Bubblewrap (`bwrap`).
//!
//! This is a **dependency-light** backend: instead of pulling the `landlock` +
//! `seccompiler` crates that codex's `linux-sandbox` crate uses, we wrap the
//! system `bwrap` (bubblewrap) binary as a subprocess. `bwrap` ships on most
//! modern Linux distros and gives honest enforcement using user namespaces:
//!
//! - the entire root filesystem is bound **read-only** (`--ro-bind / /`),
//! - `/dev`, `/proc`, and a `tmpfs` `/tmp` are provided,
//! - each writable root from the policy is `--bind`-mounted read-write
//!   (workspace-write only),
//! - the network namespace is dropped (`--unshare-net`) when network access is
//!   denied by the policy.
//!
//! Codex parity:
//! - `spawn_command_under_linux_sandbox` (`codex-rs/core/src/landlock.rs:22`) —
//!   codex spawns its `codex-linux-sandbox` helper (landlock+seccomp); we spawn
//!   `bwrap` with policy-derived args. Same shape: build args from the policy,
//!   then exec the child wrapping the target argv.
//! - codex also selects bubblewrap on Linux: `SYSTEM_BWRAP_PROGRAM = "bwrap"`
//!   and the user-namespace probe `--unshare-user --unshare-net --ro-bind / /
//!   /bin/true` (`codex-rs/sandboxing/src/bwrap.rs:15,74-83`); `find_bwrap`
//!   here mirrors `find_system_bwrap_in_path` (`bwrap.rs:168`).
//!
//! HONESTY: if `bwrap` is absent (or otherwise unusable) this backend returns a
//! typed [`LinuxSandboxError::Unavailable`] and enforces **nothing** — callers
//! must degrade to `SandboxType::None` + a logged denial rather than claim
//! enforcement. PARITY DEBT: this is *not* landlock+seccomp; it applies no
//! syscall filter, so it is coarser than codex's Linux sandbox (e.g. it cannot
//! allow read of `/` while denying specific syscalls). It does enforce the
//! FS-write and network dimensions of the policy.

use std::path::PathBuf;

use crate::sandbox_backends::policy::{FsPolicy, SandboxPolicy};

/// Absolute path probing order for the `bwrap` binary.
///
/// Codex parity: `SYSTEM_BWRAP_PROGRAM = "bwrap"` resolved on PATH
/// (`sandboxing/src/bwrap.rs:15,168`); we probe the two conventional locations.
const BWRAP_CANDIDATES: [&str; 2] = ["/usr/bin/bwrap", "/bin/bwrap"];

/// Why a Linux sandbox spawn could not be performed under enforcement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinuxSandboxError {
    /// No usable `bwrap` binary was found on this host.
    ///
    /// The caller MUST degrade to no-sandbox + a logged denial; this backend
    /// does NOT silently run the command unsandboxed.
    Unavailable { reason: String },
    /// `bwrap` was found but spawning the child failed.
    Spawn { reason: String },
}

impl std::fmt::Display for LinuxSandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { reason } => write!(f, "linux sandbox unavailable: {reason}"),
            Self::Spawn { reason } => write!(f, "linux sandbox spawn failed: {reason}"),
        }
    }
}

impl std::error::Error for LinuxSandboxError {}

/// Locate a usable `bwrap` binary, returning its path.
///
/// Codex parity: `find_system_bwrap_in_path` (`sandboxing/src/bwrap.rs:168`).
pub fn find_bwrap() -> Option<PathBuf> {
    BWRAP_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

/// Runtime capability check: is the real Linux backend available on this host?
///
/// True iff a `bwrap` binary exists. (We deliberately do not probe whether user
/// namespaces are permitted here — that is exercised by an actual spawn in the
/// gated test — but `bwrap` presence is the honest gate for `resolve()`.)
pub fn is_available() -> bool {
    find_bwrap().is_some()
}

/// Build the `bwrap` argument vector for `policy`, ending with the `--`
/// separator (the target argv is appended after).
///
/// Codex parity (arg shape): `--ro-bind / /` + per-root binds + `--unshare-net`
/// (`sandboxing/src/bwrap.rs:74-83`).
pub fn build_bwrap_args(policy: &SandboxPolicy) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    // Lock the entire root filesystem read-only by default.
    args.push("--ro-bind".to_string());
    args.push("/".to_string());
    args.push("/".to_string());
    // Essential virtual filesystems.
    args.push("--dev".to_string());
    args.push("/dev".to_string());
    args.push("--proc".to_string());
    args.push("/proc".to_string());
    args.push("--tmpfs".to_string());
    args.push("/tmp".to_string());
    // Writable roots (workspace-write only).
    if matches!(policy.fs, FsPolicy::WorkspaceWrite) {
        for root in &policy.writable_roots {
            let root = root.to_string_lossy().to_string();
            args.push("--bind".to_string());
            args.push(root.clone());
            args.push(root);
        }
    }
    // Network isolation: drop the network namespace when network is denied.
    if !policy.network_access {
        args.push("--unshare-net".to_string());
    }
    args.push("--".to_string());
    args
}

/// A prepared `bwrap` invocation: program + full argument vector (including the
/// target argv after `--`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BwrapCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Assemble a [`BwrapCommand`] running `command` (argv) under `policy`.
///
/// Returns `None` when no `bwrap` binary is available.
pub fn bwrap_command(policy: &SandboxPolicy, command: &[String]) -> Option<BwrapCommand> {
    let program = find_bwrap()?;
    let mut args = build_bwrap_args(policy);
    args.extend(command.iter().cloned());
    Some(BwrapCommand { program, args })
}

/// Spawn `command` under the Linux (`bwrap`) sandbox enforcing `policy`.
///
/// On success returns the running child. On `bwrap` absence returns
/// [`LinuxSandboxError::Unavailable`] (the caller MUST NOT then run the command
/// unsandboxed without going through the explicit no-sandbox path).
///
/// Process hardening (codex parity: the per-attempt scrubbing applied around the
/// sandboxed child — `env_clear()` then re-apply only the supplied `env`, and
/// redirect `stdin` to `/dev/null`). `bwrap` itself runs the child in fresh
/// user/pid/(net) namespaces.
pub fn spawn_command_under_linux_sandbox(
    policy: &SandboxPolicy,
    command: Vec<String>,
    cwd: PathBuf,
    env: std::collections::HashMap<String, String>,
) -> Result<std::process::Child, LinuxSandboxError> {
    let prepared =
        bwrap_command(policy, &command).ok_or_else(|| LinuxSandboxError::Unavailable {
            reason: "no `bwrap` binary found in /usr/bin or /bin".to_string(),
        })?;

    let mut cmd = std::process::Command::new(&prepared.program);
    cmd.args(&prepared.args);
    cmd.current_dir(cwd);
    // Hardening: scrub ambient env, supply only the caller's env.
    cmd.env_clear();
    cmd.envs(env);
    cmd.stdin(std::process::Stdio::null());

    cmd.spawn().map_err(|e| LinuxSandboxError::Spawn {
        reason: e.to_string(),
    })
}
