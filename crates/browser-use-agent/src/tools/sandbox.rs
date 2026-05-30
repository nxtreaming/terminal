//! Sandbox seam (stub now; real Landlock/seccomp later).

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxType {
    None,
    Restricted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxPreference {
    Auto,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxPermissions {
    UseDefault,
    RequireEscalated,
    WithAdditionalPermissions,
}

impl SandboxPermissions {
    pub fn requires_escalated_permissions(&self) -> bool {
        matches!(self, Self::RequireEscalated)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxOverride {
    NoOverride,
    BypassSandboxFirstAttempt,
}

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

pub struct SandboxLaunch {
    pub sandbox: SandboxType,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}

pub trait SandboxProvider: Send + Sync {
    fn select_initial(
        &self,
        fs: &FileSystemSandboxPolicy,
        pref: SandboxPreference,
        managed_network: bool,
    ) -> SandboxType;
    fn prepare(&self, sandbox: SandboxType, cwd: &Path, perms: SandboxPermissions)
        -> SandboxLaunch;
}

/// Stub: `select_initial -> None`, `prepare -> unsandboxed`.
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
