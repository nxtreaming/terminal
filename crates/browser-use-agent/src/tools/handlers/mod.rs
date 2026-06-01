//! Concrete tool handlers for the async agent engine.
//!
//! Each handler implements the [`ToolRuntime`](crate::tools::runtime::ToolRuntime)
//! trait stack ([`Approvable`](crate::tools::runtime::Approvable) +
//! [`Sandboxable`](crate::tools::runtime::Sandboxable) +
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime)) so it can be driven by
//! the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator).

pub mod apply_patch;
pub mod browser;
pub mod capture;
pub mod done;
pub mod goal;
pub mod mcp;
pub mod python;
pub mod request_user_input;
pub mod shell;
pub mod subagent;
pub mod tool_search;
pub mod update_plan;
pub mod view_image;
pub mod web_search;

#[cfg(test)]
mod apply_patch_tests;
#[cfg(test)]
mod browser_tests;
#[cfg(test)]
mod done_tests;
#[cfg(test)]
mod mcp_tests;
#[cfg(test)]
mod python_tests;
#[cfg(test)]
mod request_user_input_tests;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod tool_search_tests;
#[cfg(test)]
mod update_plan_tests;
#[cfg(test)]
mod view_image_tests;
#[cfg(test)]
mod web_search_tests;

pub use apply_patch::{ApplyPatchApprovalKey, ApplyPatchRequest, ApplyPatchTool};
pub use browser::{BrowserAction, BrowserApprovalKey, BrowserRequest, BrowserTool};
pub use capture::{
    CaptureCurationApprovalKey, CaptureCurationFrame, CaptureCurationRequest, CaptureCurationTool,
};
pub use done::{DoneApprovalKey, DoneRequest, DoneTool, DONE_STDOUT_PREFIX, DONE_TOOL_NAME};
pub use mcp::{
    mcp_result_tool_content, McpApprovalKey, McpCallResult, McpClient, McpTool, McpToolCallRequest,
    MCP_ERROR_EXIT_CODE, MCP_EVENT_RESULT_MAX_CHARS,
};
pub use python::{PythonApprovalKey, PythonBackend, PythonRequest, PythonTool};
pub use request_user_input::{
    RequestUserInputApprovalKey, RequestUserInputRequest, RequestUserInputResponse,
    RequestUserInputTool, UserInputAnswer, UserInputOption, UserInputQuestion,
};
pub use shell::{ShellApprovalKey, ShellRequest, ShellTool};
pub use subagent::{
    ListAgentsRequest, ListAgentsTool, SendInputRequest, SendInputTool, SpawnAgentTool,
    SubagentToolDeps, WaitAgentRequest, WaitAgentTool,
};
pub use tool_search::{
    ToolSearchApprovalKey, ToolSearchEntry, ToolSearchMatch, ToolSearchRequest, ToolSearchTool,
};
pub use update_plan::{
    PlanItem, PlanStatus, UpdatePlanApprovalKey, UpdatePlanRequest, UpdatePlanTool,
};
pub use view_image::{ViewImageApprovalKey, ViewImageRequest, ViewImageTool};
pub use web_search::{
    web_search_action_detail, web_search_detail, WebSearchAction, WebSearchApprovalKey,
    WebSearchConfig, WebSearchMode, WebSearchRequest, WebSearchTool,
};
