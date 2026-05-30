//! JSON-RPC 2.0 envelope and the subset of MCP message shapes we exchange.
//!
//! We hand-roll the protocol (we do NOT vendor `rmcp`); the wire spec is the
//! parity target. Wire shapes mirror the legacy in-house client
//! (`browser-use-core/src/mcp.rs`) — request builders (`:576-613`), the
//! `initialize` handshake (`:776-804`), the `CallToolResult` output schema
//! (`:1059-1078`), and tests asserting `structuredContent`/`isError`
//! (`:1445-1487`). MCP uses camelCase on the wire (`inputSchema`, `isError`,
//! `structuredContent`, `nextCursor`, `_meta`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::handlers::mcp::McpCallResult;

/// MCP protocol version advertised in the `initialize` handshake.
///
/// Matches the legacy in-house client's `MCP_PROTOCOL_VERSION`
/// (`browser-use-core/src/mcp.rs:16` = `"2024-11-05"`). Codex's rmcp pins a
/// newer `ProtocolVersion`, but the legacy in-house client is THIS product's
/// behavioral parity target, so we match its version.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC request id: string or number. We only ever originate numeric ids
/// (legacy `next_request_id` is an `i64`, `browser-use-core/src/mcp.rs:812`), but
/// a server may echo either, and a server->client request (elicitation) may
/// carry a string id we must echo back verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl RequestId {
    pub fn from_i64(value: i64) -> Self {
        RequestId::Number(value)
    }
}

/// Outbound JSON-RPC request (client -> server).
#[derive(Clone, Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

/// Outbound JSON-RPC notification (no id, no response expected).
#[derive(Clone, Debug, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC error object.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A message read off the wire. It can be a response to one of our requests
/// (carries an `id` plus `result`/`error`), a server-originated request (carries
/// an `id` plus `method`, e.g. `elicitation/create`), or a notification (no
/// `id`). We parse permissively and let the transport route it.
#[derive(Clone, Debug, Deserialize)]
pub struct JsonRpcMessage {
    #[serde(default)]
    pub id: Option<RequestId>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcMessage {
    /// True when this is a server-originated request (has both an id and a
    /// method) rather than a response to one of our requests.
    pub fn is_server_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }

    /// True when this is a response to one of our requests (has an id but no
    /// method).
    pub fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_none()
    }
}

/// A single tool descriptor from `tools/list`. Field names mirror the legacy
/// discovery code (`browser-use-core/src/mcp.rs:306-329`: `name`,
/// `description`, `inputSchema`, `annotations`).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

impl McpToolInfo {
    /// Whether the server advertised this tool as read-only (parallel-safe).
    /// Mirrors legacy `mcp_tool_read_only_hint` reading
    /// `annotations.readOnlyHint` (`browser-use-core/src/mcp.rs:337`,
    /// test `:1840`).
    pub fn read_only_hint(&self) -> bool {
        self.annotations
            .as_ref()
            .and_then(|a| a.get("readOnlyHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// `tools/list` result: `{ tools: [...], nextCursor }`. Mirrors the legacy
/// extraction of `result.tools` (`browser-use-core/src/mcp.rs:451-455`) and
/// `nextCursor` handling (`:549-557`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<McpToolInfo>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

/// `tools/call` result. Shape mirrors the legacy output schema
/// (`browser-use-core/src/mcp.rs:1059-1078`: `content`, `structuredContent`,
/// `isError`, `_meta`) and codex `CallToolResult`. `content` is left as raw
/// `Value`s with a `"type"` discriminator (text/image/...); the model-facing
/// handler flattens them (see `tools/handlers/mcp.rs::mcp_result_tool_content`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
    #[serde(default, rename = "structuredContent")]
    pub structured_content: Option<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

impl CallToolResult {
    /// Build the model-facing seam type ([`McpCallResult`]) from a wire
    /// `tools/call` result. Content items pass through unflattened (the handler
    /// flattens them).
    pub fn into_seam(self) -> McpCallResult {
        McpCallResult {
            content: self.content,
            is_error: self.is_error,
            structured_content: self.structured_content,
        }
    }
}

/// Build the `initialize` request params. Mirrors legacy
/// `browser-use-core/src/mcp.rs:784-791` (protocolVersion + capabilities +
/// clientInfo), but advertises an empty `elicitation` capability so a server may
/// send us `elicitation/create` (which we decline).
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "elicitation": {}
        },
        "clientInfo": {
            "name": "browser-use",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Build the `tools/call` request params. Mirrors legacy
/// `browser-use-core/src/mcp.rs:584-592`: `{ name, arguments }` with `arguments`
/// defaulting to `null` when absent.
pub fn call_tool_params(tool: &str, arguments: Option<Value>) -> Value {
    json!({
        "name": tool,
        "arguments": arguments.unwrap_or(Value::Null),
    })
}

/// The `notifications/initialized` notification sent after a successful
/// `initialize` (legacy `browser-use-core/src/mcp.rs:795-802`).
pub fn initialized_notification() -> JsonRpcNotification {
    JsonRpcNotification::new("notifications/initialized", None)
}

/// Build the result payload for a server `elicitation/create` request. We have
/// no interactive handler, so we always DECLINE. The wire shape mirrors codex's
/// elicitation result `{ "action": <action> }`
/// (`rmcp-client/src/elicitation_client_service.rs:124-150,197`).
pub fn elicitation_decline_result() -> Value {
    json!({ "action": "decline" })
}

/// Extra HTTP headers map alias, shared by the transports.
pub type HeaderMap = HashMap<String, String>;
