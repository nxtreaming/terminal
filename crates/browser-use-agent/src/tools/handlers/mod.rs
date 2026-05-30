//! Concrete tool handlers for the async agent engine.
//!
//! Each handler implements the [`ToolRuntime`](crate::tools::runtime::ToolRuntime)
//! trait stack ([`Approvable`](crate::tools::runtime::Approvable) +
//! [`Sandboxable`](crate::tools::runtime::Sandboxable) +
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime)) so it can be driven by
//! the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator).

pub mod shell;

#[cfg(test)]
mod shell_tests;

pub use shell::{ShellApprovalKey, ShellRequest, ShellTool};
