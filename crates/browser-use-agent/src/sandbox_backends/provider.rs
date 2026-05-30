//! `PlatformSandboxProvider` + the `get_platform_sandbox` / `spawn_under_sandbox`
//! dispatch (codex parity).
//!
//! This wires the real backends ([`crate::sandbox_backends::linux`],
//! [`crate::sandbox_backends::seatbelt`]) behind the **frozen**
//! [`crate::tools::sandbox::SandboxProvider`] seam. The orchestrator is NOT yet
//! flipped from `NoneSandboxProvider` to this provider — that is the later
//! guardian + approval-wiring WP. This WP only delivers the backends + provider.
//!
//! Codex parity:
//! - `get_platform_sandbox` (`codex-rs/sandboxing/src/manager.rs:48`): macOS ->
//!   Seatbelt, Linux -> seccomp/landlock (here: the `bwrap` backend), else
//!   `None`.
//! - `SandboxManager::select_initial` (`manager.rs:139`) with
//!   `SandboxablePreference { Auto, Require, Forbid }` (`manager.rs:42`); the
//!   frozen seam here collapses that to `SandboxPreference { Auto, Never }`.
//! - the per-platform spawn dispatch mirrors `SandboxManager::transform`'s
//!   per-`SandboxType` arms (`manager.rs:193-245`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::sandbox_backends::linux;
use crate::sandbox_backends::policy::{policy_summary, SandboxPolicy};
use crate::sandbox_backends::seatbelt;
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, SandboxLaunch, SandboxPermissions, SandboxPreference, SandboxProvider,
    SandboxType,
};
use crate::tools::{ExecOutput, SandboxDenial};

/// The concrete OS sandbox mechanism selected for the current platform.
///
/// Codex parity: `SandboxType { None, MacosSeatbelt, LinuxSeccomp, .. }`
/// (`sandboxing/src/manager.rs:23`). Distinct from the frozen seam's
/// two-variant [`crate::tools::sandbox::SandboxType`]; this carries the platform
/// backend identity used by the dispatcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformSandbox {
    /// macOS Seatbelt (`sandbox-exec`).
    MacosSeatbelt,
    /// Linux sandbox (here: Bubblewrap; codex uses landlock+seccomp).
    LinuxBwrap,
}

/// Select the platform sandbox backend for the current OS, if one exists.
///
/// Codex parity: `get_platform_sandbox` (`sandboxing/src/manager.rs:48`). macOS
/// picks Seatbelt, Linux picks the (bwrap) Linux backend, every other platform
/// gets `None`.
pub fn get_platform_sandbox() -> Option<PlatformSandbox> {
    if cfg!(target_os = "macos") {
        Some(PlatformSandbox::MacosSeatbelt)
    } else if cfg!(target_os = "linux") {
        Some(PlatformSandbox::LinuxBwrap)
    } else {
        None
    }
}

/// Whether a real, *enforcing* backend is available right now on this host.
///
/// HONEST: this is not just "what OS is this" — it also runtime-checks backend
/// capability (e.g. `bwrap` presence on Linux). When this returns false, the
/// provider returns [`SandboxType::None`] rather than a false `Restricted`.
pub fn real_backend_available() -> bool {
    match get_platform_sandbox() {
        Some(PlatformSandbox::MacosSeatbelt) => seatbelt::is_available(),
        Some(PlatformSandbox::LinuxBwrap) => linux::is_available(),
        None => false,
    }
}

/// Error from [`spawn_under_sandbox`] when the platform/feature is unavailable
/// or the spawn itself failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnUnderSandboxError {
    /// No enforcing backend for this platform/host (degrade to no-sandbox).
    Unavailable { reason: String },
    /// The backend exists but spawning failed.
    Spawn { reason: String },
}

impl SpawnUnderSandboxError {
    /// The human-readable reason carried by this error.
    pub fn reason(&self) -> &str {
        match self {
            Self::Unavailable { reason } | Self::Spawn { reason } => reason,
        }
    }

    /// Convert into the seam's [`SandboxDenial`] for the orchestrator's denial
    /// path. Codex parity: a failed sandbox attempt surfaces as a denial the
    /// caller can choose to retry under `SandboxType::None`. The reason is
    /// carried on the denial's [`ExecOutput::stderr`] (the seam's denial type
    /// has no free-form reason field).
    pub fn into_denial(self) -> SandboxDenial {
        let stderr = match &self {
            Self::Unavailable { reason } => format!("sandbox unavailable: {reason}"),
            Self::Spawn { reason } => format!("sandbox spawn failed: {reason}"),
        };
        SandboxDenial {
            output: ExecOutput {
                exit_code: -1,
                stdout: String::new(),
                stderr,
            },
            network_policy_decision: None,
        }
    }
}

impl std::fmt::Display for SpawnUnderSandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { reason } => write!(f, "sandbox unavailable: {reason}"),
            Self::Spawn { reason } => write!(f, "sandbox spawn failed: {reason}"),
        }
    }
}

impl std::error::Error for SpawnUnderSandboxError {}

/// Spawn `command` under the current platform's sandbox enforcing `policy`.
///
/// Routes to the Linux (`bwrap`) or macOS (Seatbelt) backend per
/// [`get_platform_sandbox`]. Returns [`SpawnUnderSandboxError::Unavailable`]
/// (NOT an unsandboxed child) when no enforcing backend exists — the caller is
/// responsible for the explicit no-sandbox fallback path.
///
/// Codex parity: the dispatch in `SandboxManager::transform` (`manager.rs:193`).
pub fn spawn_under_sandbox(
    policy: &SandboxPolicy,
    command: Vec<String>,
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<std::process::Child, SpawnUnderSandboxError> {
    match get_platform_sandbox() {
        Some(PlatformSandbox::LinuxBwrap) => {
            linux::spawn_command_under_linux_sandbox(policy, command, cwd, env).map_err(|e| match e
            {
                linux::LinuxSandboxError::Unavailable { reason } => {
                    SpawnUnderSandboxError::Unavailable { reason }
                }
                linux::LinuxSandboxError::Spawn { reason } => {
                    SpawnUnderSandboxError::Spawn { reason }
                }
            })
        }
        Some(PlatformSandbox::MacosSeatbelt) => {
            seatbelt::spawn_command_under_seatbelt(policy, command, cwd, env).map_err(|e| match e {
                seatbelt::SeatbeltError::Unsupported { reason } => {
                    SpawnUnderSandboxError::Unavailable { reason }
                }
                seatbelt::SeatbeltError::Spawn { reason } => {
                    SpawnUnderSandboxError::Spawn { reason }
                }
            })
        }
        None => Err(SpawnUnderSandboxError::Unavailable {
            reason: "no platform sandbox backend for this OS".to_string(),
        }),
    }
}

/// The real platform-backed sandbox provider.
///
/// Implements the frozen [`SandboxProvider`] seam. `select_initial` returns
/// [`SandboxType::Restricted`] only when a real enforcing backend is available
/// AND the caller did not opt out (`SandboxPreference::Never`); otherwise it is
/// HONESTLY [`SandboxType::None`].
///
/// NOTE: not yet wired as the orchestrator default — that flip is the later
/// guardian WP. Constructed but inert behind the seam for now.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformSandboxProvider;

impl PlatformSandboxProvider {
    /// Diagnostic summary of what this provider would do on this host, for logs.
    pub fn availability_summary(&self) -> String {
        match get_platform_sandbox() {
            Some(PlatformSandbox::LinuxBwrap) => {
                if linux::is_available() {
                    "linux/bwrap available".to_string()
                } else {
                    "linux/bwrap unavailable (no bwrap binary)".to_string()
                }
            }
            Some(PlatformSandbox::MacosSeatbelt) => {
                if seatbelt::is_available() {
                    "macos/seatbelt available".to_string()
                } else {
                    "macos/seatbelt unavailable".to_string()
                }
            }
            None => "no platform sandbox backend".to_string(),
        }
    }
}

impl SandboxProvider for PlatformSandboxProvider {
    fn select_initial(
        &self,
        _fs: &FileSystemSandboxPolicy,
        pref: SandboxPreference,
        _managed_network: bool,
    ) -> SandboxType {
        // `Never` opts the tool out entirely (codex `SandboxablePreference::Forbid`).
        if matches!(pref, SandboxPreference::Never) {
            return SandboxType::None;
        }
        // HONEST: only claim Restricted when a real backend can enforce here;
        // otherwise honestly degrade to None (no `tracing` dep in this crate, so
        // the degrade is surfaced via the resolved value / `availability_summary`
        // rather than a log line).
        if real_backend_available() {
            SandboxType::Restricted
        } else {
            SandboxType::None
        }
    }

    fn prepare(
        &self,
        sandbox: SandboxType,
        _cwd: &Path,
        _perms: SandboxPermissions,
    ) -> SandboxLaunch {
        // The actual command-spawning under the sandbox is done by the runtime
        // via [`spawn_under_sandbox`]; `prepare` only resolves the launch handle.
        // If a Restricted launch is requested but no backend can enforce, we
        // honestly downgrade the launch to None (the orchestrator would
        // otherwise hold a misleading Restricted handle).
        // If a Restricted launch is requested but no backend can enforce, honestly
        // downgrade to None (surfaced via the resolved value, not a log line —
        // this crate has no `tracing` dependency).
        let resolved = match sandbox {
            SandboxType::Restricted if real_backend_available() => SandboxType::Restricted,
            SandboxType::Restricted => SandboxType::None,
            SandboxType::None => SandboxType::None,
        };
        SandboxLaunch {
            sandbox: resolved,
            cancel: None,
        }
    }
}

/// Build a [`SandboxDenial`] describing why enforcement was unavailable, for the
/// no-sandbox degrade path. The reason + policy summary are carried on the
/// denial's [`ExecOutput::stderr`].
pub fn unavailable_denial(policy: &SandboxPolicy, reason: &str) -> SandboxDenial {
    SandboxDenial {
        output: ExecOutput {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!(
                "sandbox enforcement unavailable ({reason}); policy was [{}]",
                policy_summary(policy)
            ),
        },
        network_policy_decision: None,
    }
}
