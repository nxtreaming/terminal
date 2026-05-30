//! macOS Seatbelt sandbox backend (`sandbox-exec` + generated profile).
//!
//! On macOS this generates a Seatbelt (`.sb`) profile from a [`SandboxPolicy`]
//! and spawns the target command under `/usr/bin/sandbox-exec`. On every other
//! platform the spawn path is a compiled stub that reports "unsupported on this
//! platform" so the dispatcher degrades honestly (the profile generation itself
//! is platform-independent and stays compiled everywhere so it type-checks and
//! can be unit-tested on Linux).
//!
//! Codex parity:
//! - `MACOS_PATH_TO_SEATBELT_EXECUTABLE` + `create_seatbelt_command_args`
//!   (referenced from `codex-rs/sandboxing/src/manager.rs:197-213`,
//!   `#[cfg(target_os = "macos")] crate::seatbelt`), driven by the
//!   `restricted_read_only` / `seatbelt_base_policy` `.sbpl` profiles in
//!   `codex-rs/sandboxing/src/`.
//! - the dispatcher gates Seatbelt to macOS and returns
//!   `SandboxTransformError::SeatbeltUnavailable` elsewhere
//!   (`sandboxing/src/manager.rs:215-216`); the stub here mirrors that.
//!
//! PARITY DEBT: the real `sandbox-exec` path is only reachable on macOS; on this
//! Linux box it is a typed-unsupported stub (we never run on macOS here). The
//! profile is a codex-minimal one (`deny default`, allow read, scoped writes,
//! network toggle); codex's production `.sbpl` profiles are richer.

use std::path::PathBuf;

use crate::sandbox_backends::policy::{FsPolicy, SandboxPolicy};

/// Why a Seatbelt spawn could not be performed under enforcement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeatbeltError {
    /// Seatbelt is not available on this platform (non-macOS).
    Unsupported { reason: String },
    /// `sandbox-exec` was found but spawning the child failed.
    Spawn { reason: String },
}

impl std::fmt::Display for SeatbeltError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { reason } => write!(f, "seatbelt unsupported: {reason}"),
            Self::Spawn { reason } => write!(f, "seatbelt spawn failed: {reason}"),
        }
    }
}

impl std::error::Error for SeatbeltError {}

/// Generate a Seatbelt (`.sb`) profile string for `policy`.
///
/// Read-only denies all writes; workspace-write allows writes under each
/// writable root. Network is denied unless `network_access` is set.
///
/// Codex parity: the generated Seatbelt policy text (codex builds it from the
/// `.sbpl` base + writable-root / network clauses in `create_seatbelt_command_args`).
/// Compiled on all platforms (pure string building) so it can be unit-tested on
/// Linux.
pub fn seatbelt_profile(policy: &SandboxPolicy) -> String {
    let mut profile = String::new();
    profile.push_str("(version 1)\n");
    profile.push_str("(deny default)\n");
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow file-read*)\n");
    match policy.fs {
        FsPolicy::ReadOnly => {
            profile.push_str("(deny file-write*)\n");
        }
        FsPolicy::WorkspaceWrite => {
            for root in &policy.writable_roots {
                profile.push_str(&format!(
                    "(allow file-write* (subpath \"{}\"))\n",
                    root.to_string_lossy()
                ));
            }
        }
    }
    if policy.network_access {
        profile.push_str("(allow network*)\n");
    } else {
        profile.push_str("(deny network*)\n");
    }
    profile
}

/// A prepared `sandbox-exec` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatbeltCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Assemble a `sandbox-exec` command running `command` under `policy`.
///
/// Codex parity: `create_seatbelt_command_args` prepends
/// `MACOS_PATH_TO_SEATBELT_EXECUTABLE` + the profile (`manager.rs:201-212`).
pub fn seatbelt_command(policy: &SandboxPolicy, command: &[String]) -> SeatbeltCommand {
    let profile = seatbelt_profile(policy);
    let mut args = vec!["-p".to_string(), profile, "--".to_string()];
    args.extend(command.iter().cloned());
    SeatbeltCommand {
        program: PathBuf::from("/usr/bin/sandbox-exec"),
        args,
    }
}

/// Whether the Seatbelt backend can actually enforce on this platform.
///
/// True only on macOS. HONEST: this is a compile-time platform gate, not a
/// pretend "available everywhere".
pub fn is_available() -> bool {
    cfg!(target_os = "macos")
}

/// Spawn `command` under macOS Seatbelt using the policy-derived profile.
///
/// Codex parity: the Seatbelt dispatch in `SandboxManager::transform`
/// (`manager.rs:196-214`), including per-attempt `env_clear()` +
/// `stdin(Stdio::null())` hardening.
#[cfg(target_os = "macos")]
pub fn spawn_command_under_seatbelt(
    policy: &SandboxPolicy,
    command: Vec<String>,
    cwd: PathBuf,
    env: std::collections::HashMap<String, String>,
) -> Result<std::process::Child, SeatbeltError> {
    use std::process::Stdio;

    let prepared = seatbelt_command(policy, &command);
    let mut cmd = std::process::Command::new(&prepared.program);
    cmd.args(&prepared.args);
    cmd.current_dir(cwd);
    cmd.env_clear();
    cmd.envs(env);
    cmd.stdin(Stdio::null());
    cmd.spawn().map_err(|e| SeatbeltError::Spawn {
        reason: e.to_string(),
    })
}

/// Compiled stub for non-macOS platforms: Seatbelt cannot enforce here.
///
/// HONEST DEGRADATION: returns [`SeatbeltError::Unsupported`] rather than
/// running the command unsandboxed. Keeps the signature identical to the macOS
/// path so the dispatcher type-checks on every platform. Codex parity:
/// `SandboxTransformError::SeatbeltUnavailable` (`manager.rs:216`).
#[cfg(not(target_os = "macos"))]
pub fn spawn_command_under_seatbelt(
    _policy: &SandboxPolicy,
    _command: Vec<String>,
    _cwd: PathBuf,
    _env: std::collections::HashMap<String, String>,
) -> Result<std::process::Child, SeatbeltError> {
    Err(SeatbeltError::Unsupported {
        reason: "macOS Seatbelt is only available on macOS".to_string(),
    })
}
