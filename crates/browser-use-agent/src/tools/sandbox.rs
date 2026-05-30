//! Sandbox policy and provider seam (codex `sandboxing` parity).
//!
//! The point of this module is the [`SandboxProvider`] trait: a tool is written
//! once against [`SandboxType`] / [`SandboxPermissions`] and runs unchanged
//! whether the active provider is the [`NoneSandboxProvider`] stub (today) or a
//! real Landlock/seccomp sandbox dropped in later. The enums are pure value
//! types; the [`NoneSandboxProvider`] never sandboxes anything.

use std::path::Path;

/// Concrete sandbox flavor selected for an attempt.
///
/// Codex parity: `SandboxType`. `Restricted` stands in for the real
/// Landlock/seccomp profile that lands later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxType {
    /// Run with no sandbox.
    None,
    /// Run inside the restricted sandbox.
    Restricted,
}

/// How strongly a tool prefers to run inside the sandbox.
///
/// Codex parity: `SandboxablePreference`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxPreference {
    /// Let the provider pick based on policy.
    Auto,
    /// Never sandbox this tool.
    Never,
}

/// Per-request sandbox permission selection.
///
/// Codex parity: `SandboxPermissions`. `RequireEscalated` is the signal that the
/// first attempt should bypass the sandbox (see
/// [`crate::tools::runtime::sandbox_override_for_first_attempt`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxPermissions {
    /// Use the ambient sandbox policy unchanged.
    UseDefault,
    /// Require escalated (no-sandbox) execution.
    RequireEscalated,
    /// Run sandboxed but with additional grants.
    WithAdditionalPermissions,
}

impl SandboxPermissions {
    pub fn requires_escalated_permissions(&self) -> bool {
        matches!(self, Self::RequireEscalated)
    }
}

/// A per-attempt override of the configured sandbox decision.
///
/// Codex parity: `SandboxOverride` (sandboxing.rs:240-244).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxOverride {
    /// Use whatever the configured policy / provider chooses.
    NoOverride,
    /// Force no sandbox for the first attempt.
    BypassSandboxFirstAttempt,
}

/// The resolved filesystem sandbox policy for the turn.
///
/// Codex parity: `FileSystemSandboxPolicy` (codex_protocol::permissions). The
/// real codex type carries a richer `kind`; here `restricted` collapses
/// `FileSystemSandboxKind::Restricted` and `denied_read` flags the deny-read
/// special case that suppresses escalation in
/// [`crate::tools::runtime::sandbox_override_for_first_attempt`].
#[derive(Clone, Debug)]
pub struct FileSystemSandboxPolicy {
    pub restricted: bool,
    pub denied_read: bool,
}

impl FileSystemSandboxPolicy {
    pub fn is_restricted(&self) -> bool {
        self.restricted
    }

    pub fn has_denied_read_restrictions(&self) -> bool {
        self.denied_read
    }
}

/// A prepared sandbox ready for an attempt to run inside.
///
/// Codex parity: the launch handle returned by `SandboxManager`.
pub struct SandboxLaunch {
    pub sandbox: SandboxType,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}

/// Prepares a sandbox for a tool attempt.
///
/// This is *the* seam: a tool runs identically whatever the live provider is.
/// Codex parity: `SandboxManager` (`select_initial` / `transform`).
pub trait SandboxProvider: Send + Sync {
    /// Choose the initial sandbox type for an attempt.
    fn select_initial(
        &self,
        fs: &FileSystemSandboxPolicy,
        pref: SandboxPreference,
        managed_network: bool,
    ) -> SandboxType;
    /// Resolve a chosen sandbox type into a runnable [`SandboxLaunch`].
    fn prepare(&self, sandbox: SandboxType, cwd: &Path, perms: SandboxPermissions)
        -> SandboxLaunch;
}

/// The no-op provider: never sandboxes anything.
///
/// The initial drop-in so a tool can be written once and run under `sandbox =
/// None` today, with a real sandbox swapping in later behind the same trait.
/// `select_initial -> SandboxType::None`, `prepare -> unsandboxed`.
pub struct NoneSandboxProvider;

impl SandboxProvider for NoneSandboxProvider {
    fn select_initial(
        &self,
        _fs: &FileSystemSandboxPolicy,
        _p: SandboxPreference,
        _m: bool,
    ) -> SandboxType {
        SandboxType::None
    }

    fn prepare(&self, _s: SandboxType, _cwd: &Path, _p: SandboxPermissions) -> SandboxLaunch {
        SandboxLaunch {
            sandbox: SandboxType::None,
            cancel: None,
        }
    }
}
