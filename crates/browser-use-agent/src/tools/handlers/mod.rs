//! Concrete tool handlers for the async agent engine.
//!
//! Each handler implements the [`ToolRuntime`](crate::tools::runtime::ToolRuntime)
//! trait stack ([`Approvable`](crate::tools::runtime::Approvable) +
//! [`Sandboxable`](crate::tools::runtime::Sandboxable) +
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime)) so it can be driven by
//! the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator).

pub mod apply_patch;
pub mod shell;

#[cfg(test)]
mod apply_patch_tests;
#[cfg(test)]
mod shell_tests;

pub use apply_patch::{ApplyPatchApprovalKey, ApplyPatchRequest, ApplyPatchTool};
pub use shell::{ShellApprovalKey, ShellRequest, ShellTool};
