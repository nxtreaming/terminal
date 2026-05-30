//! Real OS sandbox backends behind the frozen [`crate::tools::sandbox`] seam
//! (codex `sandboxing` parity).
//!
//! Today the orchestrator runs with [`crate::tools::sandbox::NoneSandboxProvider`]
//! (everything resolves to `SandboxType::None`). This module provides the real
//! machinery that a later guardian + approval-wiring WP will flip the
//! orchestrator over to:
//!
//! - [`policy`] — the [`policy::SandboxPolicy`] value model + the pure
//!   [`policy::derive_sandbox_policy`] (read-only vs workspace-write + writable
//!   roots + network toggle). Codex parity: the `SandboxPolicy` enum
//!   (`protocol.rs`) + `compatibility_workspace_write_policy`
//!   (`sandboxing/src/manager.rs`).
//! - [`linux`] — the REAL Linux backend: a dependency-light Bubblewrap (`bwrap`)
//!   subprocess wrapper that enforces the FS-write and network dimensions of the
//!   policy via user namespaces. Degrades to a typed `Unavailable` error when no
//!   `bwrap` binary is present. Codex parity (shape):
//!   `core/src/landlock.rs::spawn_command_under_linux_sandbox` +
//!   `sandboxing/src/bwrap.rs`.
//! - [`seatbelt`] — the macOS Seatbelt backend (`sandbox-exec` + generated
//!   profile), a compiled stub reporting "unsupported" on non-macOS. Codex
//!   parity: the macOS arm of `SandboxManager::transform` (`manager.rs`).
//! - [`provider`] — [`provider::PlatformSandboxProvider`] (implements the frozen
//!   [`crate::tools::sandbox::SandboxProvider`] trait, HONESTLY resolving
//!   `Restricted` only when a backend can enforce), plus
//!   [`provider::get_platform_sandbox`] and the
//!   [`provider::spawn_under_sandbox`] dispatcher. Codex parity:
//!   `sandboxing/src/manager.rs::get_platform_sandbox` + `select_initial`.
//!
//! HONESTY / PARITY DEBT: the Linux backend is `bwrap`-based, not the
//! landlock+seccomp the codex `linux-sandbox` crate uses; it enforces FS-write
//! and network restriction but applies no syscall filter. Seatbelt's real spawn
//! is macOS-only. The orchestrator flip is deferred to the guardian WP. None of
//! these paths claim enforcement they do not perform — unavailable backends
//! surface a typed denial / `SandboxType::None`.

pub mod linux;
pub mod policy;
pub mod provider;
pub mod seatbelt;

pub use policy::{derive_sandbox_policy, FsIntent, FsPolicy, SandboxPolicy};
pub use provider::{
    get_platform_sandbox, real_backend_available, spawn_under_sandbox, PlatformSandbox,
    PlatformSandboxProvider, SpawnUnderSandboxError,
};

#[cfg(test)]
mod tests;
