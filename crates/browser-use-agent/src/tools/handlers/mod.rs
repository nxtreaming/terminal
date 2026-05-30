//! Concrete tool handlers for the async agent engine.
//!
//! Each handler implements the [`ToolRuntime`](crate::tools::runtime::ToolRuntime)
//! trait stack ([`Approvable`](crate::tools::runtime::Approvable) +
//! [`Sandboxable`](crate::tools::runtime::Sandboxable) +
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime)) so it can be driven by
//! the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator).

pub mod apply_patch;
pub mod request_user_input;
pub mod shell;
pub mod update_plan;
pub mod view_image;

#[cfg(test)]
mod apply_patch_tests;
#[cfg(test)]
mod request_user_input_tests;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod update_plan_tests;
#[cfg(test)]
mod view_image_tests;

pub use apply_patch::{ApplyPatchApprovalKey, ApplyPatchRequest, ApplyPatchTool};
pub use request_user_input::{
    RequestUserInputApprovalKey, RequestUserInputRequest, RequestUserInputResponse,
    RequestUserInputTool, UserInputAnswer, UserInputOption, UserInputQuestion,
};
pub use shell::{ShellApprovalKey, ShellRequest, ShellTool};
pub use update_plan::{
    PlanItem, PlanStatus, UpdatePlanApprovalKey, UpdatePlanRequest, UpdatePlanTool,
};
pub use view_image::{ViewImageApprovalKey, ViewImageRequest, ViewImageTool};
