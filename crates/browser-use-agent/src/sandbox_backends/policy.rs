//! Sandbox policy model + `derive_sandbox_policy` (codex parity).
//!
//! Mirrors the codex `codex_sandboxing` crate shape: a [`SandboxPolicy`] is a
//! plain value type describing what an exec attempt may touch (read-only vs
//! workspace-write, the writable roots, and whether outbound network is
//! permitted), and [`derive_sandbox_policy`] turns caller intent into one.
//!
//! Codex parity:
//! - `SandboxPolicy { WorkspaceWrite { writable_roots, network_access, .. },
//!   ReadOnly, .. }` (`codex-rs/protocol/src/protocol.rs`, the
//!   `SandboxPolicy` enum) + the runtime
//!   `compatibility_workspace_write_policy` builder in
//!   `codex-rs/sandboxing/src/manager.rs:276` (cwd-rooted writable roots +
//!   `network_access`).
//! - platform selection via `get_platform_sandbox`
//!   (`codex-rs/sandboxing/src/manager.rs:48`) and `SandboxManager::select_initial`
//!   (`manager.rs:139`) with `SandboxablePreference { Auto, Require, Forbid }`
//!   (`manager.rs:42`); this crate's frozen seam collapses that to
//!   [`crate::tools::sandbox::SandboxPreference`] `{ Auto, Never }`.
//!
//! This module is pure (no I/O) and unit-tested in `tests.rs`.

use std::path::PathBuf;

/// Filesystem access mode for a [`SandboxPolicy`].
///
/// Codex parity: the `SandboxPolicy::{ReadOnly, WorkspaceWrite}` distinction
/// (`protocol.rs` `SandboxPolicy` enum).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsPolicy {
    /// Read-only: no filesystem writes permitted.
    ReadOnly,
    /// Workspace-write: writes permitted only under `writable_roots`.
    WorkspaceWrite,
}

/// Caller-supplied filesystem intent used to derive a [`SandboxPolicy`].
///
/// Codex parity: the intent that selects `SandboxPolicy::new_read_only` vs
/// `new_workspace_write` before transformation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsIntent {
    /// Read-only access.
    ReadOnly,
    /// Workspace-write access rooted at the cwd (plus extra writable roots).
    WorkspaceWrite,
}

/// Policy describing what an exec attempt may access.
///
/// Codex parity: `SandboxPolicy` (`protocol.rs`). `writable_roots` is only
/// meaningful when `fs == FsPolicy::WorkspaceWrite`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Filesystem access mode.
    pub fs: FsPolicy,
    /// Writable roots (workspace-write only).
    pub writable_roots: Vec<PathBuf>,
    /// Whether outbound network access is permitted.
    pub network_access: bool,
}

impl SandboxPolicy {
    /// A fully locked-down read-only policy with no network.
    ///
    /// Codex parity: `SandboxPolicy::ReadOnly` (`protocol.rs`).
    pub fn new_read_only() -> Self {
        Self {
            fs: FsPolicy::ReadOnly,
            writable_roots: Vec::new(),
            network_access: false,
        }
    }

    /// A workspace-write policy rooted at `cwd` plus any extra roots.
    ///
    /// Codex parity: `compatibility_workspace_write_policy`
    /// (`sandboxing/src/manager.rs:276`) — the cwd is always the first writable
    /// root, then the configured roots follow.
    pub fn new_workspace_write(cwd: PathBuf, mut extra: Vec<PathBuf>) -> Self {
        let mut writable_roots = vec![cwd];
        writable_roots.append(&mut extra);
        Self {
            fs: FsPolicy::WorkspaceWrite,
            writable_roots,
            network_access: false,
        }
    }

    /// Whether the policy grants full (unrestricted) outbound network access.
    ///
    /// Codex parity: `SandboxPolicy::has_full_network_access` (`protocol.rs`).
    pub fn has_full_network_access(&self) -> bool {
        self.network_access
    }

    /// Whether the policy permits any filesystem writes.
    pub fn allows_writes(&self) -> bool {
        matches!(self.fs, FsPolicy::WorkspaceWrite)
    }
}

/// Derive the concrete [`SandboxPolicy`] for the requested filesystem intent.
///
/// Codex parity: the policy-derivation done in
/// `SandboxManager::transform`/`compatibility_workspace_write_policy`
/// (`sandboxing/src/manager.rs`). Read-only intent yields a fully locked-down
/// policy; workspace-write yields writes confined to `cwd` +
/// `extra_writable_roots`. The `network_access` flag is applied after the
/// constructor (the constructors default it off).
pub fn derive_sandbox_policy(
    intent: FsIntent,
    cwd: PathBuf,
    extra_writable_roots: Vec<PathBuf>,
    network_access: bool,
) -> SandboxPolicy {
    match intent {
        FsIntent::ReadOnly => {
            let mut policy = SandboxPolicy::new_read_only();
            policy.network_access = network_access;
            policy
        }
        FsIntent::WorkspaceWrite => {
            let mut policy = SandboxPolicy::new_workspace_write(cwd, extra_writable_roots);
            policy.network_access = network_access;
            policy
        }
    }
}

/// Human-readable one-line summary of a [`SandboxPolicy`] for logs/denials.
pub fn policy_summary(policy: &SandboxPolicy) -> String {
    let fs = match policy.fs {
        FsPolicy::ReadOnly => "read-only",
        FsPolicy::WorkspaceWrite => "workspace-write",
    };
    let net = if policy.network_access {
        "network-allowed"
    } else {
        "network-denied"
    };
    format!("{fs}, {net}")
}
