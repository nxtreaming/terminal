//! Process-lifecycle infrastructure: crypto-provider install + exec cleanup.
//!
//! Ported faithfully from `browser-use-core`'s `lib.rs`:
//! - [`install_process_crypto_provider`] == core `install_process_crypto_provider`
//!   (`crates/browser-use-core/src/lib.rs:121`).
//! - [`UnifiedExecShutdownCleanup`] == core `UnifiedExecShutdownCleanup`
//!   (`crates/browser-use-core/src/lib.rs:22395`).
//!
//! ## Cleanup-on-drop caveat (honest)
//!
//! In core, `UnifiedExecShutdownCleanup::drop` calls
//! `cleanup_all_agent_runtime_state()`
//! (`crates/browser-use-core/src/lib.rs:22391`), which tears down unified-exec
//! command sessions and MCP connections. This crate now owns unified exec
//! cleanup; MCP cleanup is still a future integration point.

/// Install the process-wide rustls crypto provider.
///
/// Mirrors `browser-use-core::install_process_crypto_provider`
/// (`crates/browser-use-core/src/lib.rs:121`) exactly: installs the aws-lc-rs
/// default provider, ignoring the error returned when a provider is already
/// installed, so the call is idempotent across components and repeated calls.
pub fn install_process_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Run unified-exec / runtime-state shutdown cleanup.
///
/// Mirrors `browser-use-core::cleanup_all_agent_runtime_state`
/// (`crates/browser-use-core/src/lib.rs:22391`). The real implementation sums
/// `cleanup_all_unified_exec_commands()` + `mcp::cleanup_all_mcp_connections()`;
/// those modules are not yet ported here, so this returns the number of cleaned
/// resources (currently `0`) and serves as the wiring point for them. Returning
/// the count preserves the core signature shape for future callers.
fn run_shutdown_cleanup() -> usize {
    crate::entrypoint::cleanup_all_unified_exec_managers()
}

/// RAII guard that runs unified-exec shutdown cleanup when dropped.
///
/// Mirrors `browser-use-core::UnifiedExecShutdownCleanup`
/// (`crates/browser-use-core/src/lib.rs:22395`): a zero-sized guard the TUI/CLI
/// hold for the lifetime of a process/session; its `Drop` performs best-effort
/// teardown of all agent runtime state.
#[derive(Debug, Default)]
pub struct UnifiedExecShutdownCleanup;

impl UnifiedExecShutdownCleanup {
    /// Create a new cleanup guard.
    ///
    /// Mirrors `browser-use-core::UnifiedExecShutdownCleanup::new`
    /// (`crates/browser-use-core/src/lib.rs:22399`).
    pub fn new() -> Self {
        Self
    }
}

impl Drop for UnifiedExecShutdownCleanup {
    fn drop(&mut self) {
        let _ = run_shutdown_cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_provider_install_is_idempotent() {
        // Mirrors core's test (`crates/browser-use-core/src/lib.rs:44288`):
        // calling twice must not panic (the second install errors and is
        // ignored).
        install_process_crypto_provider();
        install_process_crypto_provider();
    }

    #[test]
    fn guard_constructs_and_drops_cleanly() {
        {
            let _guard = UnifiedExecShutdownCleanup::new();
        }
        // Default constructs and drops cleanly as well.
        let _ = UnifiedExecShutdownCleanup;
        let _ = UnifiedExecShutdownCleanup::default();
    }

    #[test]
    fn shutdown_cleanup_returns_count() {
        // No runtime state is wired in yet, so the cleanup count is zero.
        assert_eq!(run_shutdown_cleanup(), 0);
    }
}
