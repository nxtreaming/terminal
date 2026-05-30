//! Concrete MCP (Model Context Protocol) transports behind the
//! [`crate::tools::handlers::mcp::McpClient`] seam.
//!
//! The protocol is hand-rolled JSON-RPC 2.0 (we do NOT vendor the `rmcp` crate);
//! the wire spec is the parity target, with the legacy in-house client
//! (`browser-use-core/src/mcp.rs`) and codex's rmcp usage as references.
//!
//! - [`protocol`]: JSON-RPC envelope + MCP message shapes.
//! - [`config`]: transport-tagged server configuration.
//! - [`stdio`]: child-process stdio transport.
//! - [`http`]: streamable-HTTP (JSON or SSE) transport.
//! - [`oauth`]: PKCE helpers + token cache (interactive leg stubbed).
//! - [`manager`]: parallel-connect manager implementing the sync seam.

pub mod config;
pub mod http;
pub mod manager;
pub mod oauth;
pub mod protocol;
pub mod stdio;

pub use config::{McpServerConfig, McpServerTransport};
pub use manager::{
    fully_qualified_tool_name, parse_tool_name, ClientStartErrors, McpConnectionManager,
};
pub use protocol::{CallToolResult, ListToolsResult, McpToolInfo};

#[cfg(test)]
mod tests;
