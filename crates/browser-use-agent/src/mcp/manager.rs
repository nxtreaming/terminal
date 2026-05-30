//! [`McpConnectionManager`]: owns connected transports keyed by server name,
//! connects them in PARALLEL with per-server failure isolation, and bridges the
//! async transports to the SYNC [`McpClient`] seam.
//!
//! Parity targets:
//! - `mcp__{server}__{tool}` delimiter + qualify/parse: codex
//!   `codex-mcp/src/mcp/mod.rs:46-47` (`MCP_TOOL_NAME_DELIMITER = "__"`,
//!   `MCP_TOOL_NAME_PREFIX = "mcp"`), `:50-52` `qualify_tool_name`, `:55-61`
//!   `parse_tool_name` (strip the `mcp__` prefix, then `split_once("__")`).
//!   This matches the model-facing seam's
//!   `McpToolCallRequest::parse_namespaced` (`tools/handlers/mcp.rs:166-173`),
//!   which splits once after the server.
//! - parallel connect + failure isolation: codex
//!   `codex-mcp/src/connection_manager.rs:191` (`JoinSet`), `:259` spawn per
//!   server, `:264-274`/`:298-319` drain results recording per-server failures
//!   (`McpStartupStatus::Failed`) without aborting the others.
//! - unknown-server error + content pass-through: codex
//!   `connection_manager.rs:597`+`:686-693` (`client_by_name` → `unknown MCP
//!   server '{name}'`), `:610-624` the client's `CallToolResult` content passes
//!   straight through.
//!
//! ## Sync-over-async bridge
//! The seam method [`McpClient::call_tool`] is synchronous; the transports are
//! async. The manager owns a DEDICATED multi-thread `tokio::runtime::Runtime`
//! created at construction and `block_on`s transport futures on it. This is
//! panic-safe from ANY caller context: `block_on` on a runtime we own does not
//! nest inside the *current* runtime's worker (unlike `Handle::block_on` on the
//! ambient runtime, which panics from within an async context). The model-facing
//! `McpTool::run` (`tools/handlers/mcp.rs:494-505`) already calls the seam inside
//! `tokio::task::spawn_blocking`, so blocking is permitted there — but owning our
//! own runtime removes any dependence on the caller's context entirely, which is
//! the most robust choice and avoids `block_in_place`'s "must be on a
//! multi-thread runtime" precondition.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::mcp::config::{McpServerConfig, McpServerTransport};
use crate::mcp::http::StreamableHttpTransport;
use crate::mcp::protocol::McpToolInfo;
use crate::mcp::stdio::StdioTransport;
use crate::tools::handlers::mcp::{McpCallResult, McpClient};

/// Delimiter between the prefix/server/tool segments of a fully-qualified
/// model-visible MCP tool name. Matches codex `MCP_TOOL_NAME_DELIMITER`
/// (`codex-mcp/src/mcp/mod.rs:46`).
pub const MCP_TOOL_NAME_DELIMITER: &str = "__";

/// Prefix marking a model-visible name as an MCP tool. Matches codex
/// `MCP_TOOL_NAME_PREFIX` (`codex-mcp/src/mcp/mod.rs:47`).
pub const MCP_TOOL_NAME_PREFIX: &str = "mcp";

/// `mcp__{server}__{tool}`. Matches codex `qualify_tool_name`
/// (`codex-mcp/src/mcp/mod.rs:50-52`).
pub fn fully_qualified_tool_name(server: &str, tool: &str) -> String {
    format!(
        "{MCP_TOOL_NAME_PREFIX}{MCP_TOOL_NAME_DELIMITER}{server}{MCP_TOOL_NAME_DELIMITER}{tool}"
    )
}

/// Split a fully-qualified `mcp__{server}__{tool}` name into `(server, tool)`.
/// Matches codex `parse_tool_name` (`codex-mcp/src/mcp/mod.rs:55-61`): strip the
/// `mcp__` prefix, then `split_once("__")`. Identical to the model-facing seam's
/// `McpToolCallRequest::parse_namespaced` (`tools/handlers/mcp.rs:166-173`).
pub fn parse_tool_name(fq_name: &str) -> Option<(String, String)> {
    let suffix =
        fq_name.strip_prefix(&format!("{MCP_TOOL_NAME_PREFIX}{MCP_TOOL_NAME_DELIMITER}"))?;
    let (server, tool) = suffix.split_once(MCP_TOOL_NAME_DELIMITER)?;
    Some((server.to_string(), tool.to_string()))
}

/// A connected transport (stdio or HTTP).
enum Transport {
    Stdio(StdioTransport),
    Http(StreamableHttpTransport),
}

impl Transport {
    async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        match self {
            Transport::Stdio(t) => t.list_tools().await,
            Transport::Http(t) => t.list_tools().await,
        }
    }

    async fn call_tool(
        &self,
        tool: &str,
        args: Option<Value>,
    ) -> Result<crate::mcp::protocol::CallToolResult> {
        match self {
            Transport::Stdio(t) => t.call_tool(tool, args).await,
            Transport::Http(t) => t.call_tool(tool, args).await,
        }
    }
}

struct ConnectedServer {
    transport: Transport,
    config: McpServerConfig,
}

/// Errors recorded per server during [`McpConnectionManager::connect_all`]
/// (failure isolation: a failed server is recorded here, the rest still
/// connect). Analogous to codex's per-server startup-failure handling
/// (`connection_manager.rs:271-278`).
pub type ClientStartErrors = HashMap<String, anyhow::Error>;

pub struct McpConnectionManager {
    servers: HashMap<String, ConnectedServer>,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl McpConnectionManager {
    /// Connect every configured server IN PARALLEL, isolating per-server
    /// failures. Returns the manager plus a map of servers that failed to
    /// connect. Mirrors codex `McpConnectionManager::new`
    /// (`connection_manager.rs:171-282`): spawn each connect on a `JoinSet`, then
    /// drain results recording failures without aborting the rest.
    ///
    /// A dedicated multi-thread runtime is created and OWNED by the manager so
    /// the sync seam can `block_on` it later without nested-runtime panics.
    pub fn connect_all(
        configs: HashMap<String, McpServerConfig>,
    ) -> Result<(Self, ClientStartErrors)> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| anyhow!("building mcp runtime: {e}"))?,
        );

        let (servers, errors) = runtime.block_on(connect_all_inner(configs));
        Ok((Self { servers, runtime }, errors))
    }

    /// Number of successfully-connected servers.
    pub fn connected_count(&self) -> usize {
        self.servers.len()
    }

    /// The names of all successfully-connected servers (used by
    /// [`parse_tool_name`]).
    pub fn server_names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// List tools for one server (async), filtered by the server's
    /// enabled/disabled lists.
    pub async fn list_tools(&self, server: &str) -> Result<Vec<McpToolInfo>> {
        let connected = self
            .servers
            .get(server)
            .ok_or_else(|| anyhow!("MCP client for `{server}` not found"))?;
        let tools = connected.transport.list_tools().await?;
        Ok(tools
            .into_iter()
            .filter(|t| connected.config.is_tool_allowed(&t.name))
            .collect())
    }

    /// List all tools across all servers, keyed by fully-qualified `{server}__`
    /// name. Analogous to codex `list_all_tools` (`connection_manager.rs:372`).
    /// A server whose `tools/list` fails is skipped (isolation).
    pub async fn list_all_tools(&self) -> HashMap<String, McpToolInfo> {
        let mut all = HashMap::new();
        for (server, connected) in &self.servers {
            let Ok(tools) = connected.transport.list_tools().await else {
                continue;
            };
            for tool in tools {
                if !connected.config.is_tool_allowed(&tool.name) {
                    continue;
                }
                let fq = fully_qualified_tool_name(server, &tool.name);
                all.insert(fq, tool);
            }
        }
        all
    }

    /// Call a tool on a server (async). Mirrors codex `call_tool`
    /// (`connection_manager.rs:587-599`): unknown server → `not found` error; the
    /// `CallToolResult` content passes through unflattened (via
    /// [`crate::mcp::protocol::CallToolResult::into_seam`]).
    pub async fn call_tool_async(
        &self,
        server: &str,
        tool: &str,
        args: Option<Value>,
    ) -> Result<McpCallResult> {
        let connected = self
            .servers
            .get(server)
            .ok_or_else(|| anyhow!("MCP client for `{server}` not found"))?;
        if !connected.config.is_tool_allowed(tool) {
            return Err(anyhow!("tool `{tool}` is not enabled on server `{server}`"));
        }
        let result = connected.transport.call_tool(tool, args).await?;
        Ok(result.into_seam())
    }
}

/// Connect all servers in parallel, returning the connected set and per-server
/// errors. Mirrors codex `connection_manager.rs:191-279`.
async fn connect_all_inner(
    configs: HashMap<String, McpServerConfig>,
) -> (HashMap<String, ConnectedServer>, ClientStartErrors) {
    let mut servers = HashMap::new();
    let mut errors: ClientStartErrors = HashMap::new();

    let mut join_set: JoinSet<(String, Result<ConnectedServer>)> = JoinSet::new();
    for (name, config) in configs {
        join_set.spawn(async move {
            let result = connect_one(&config).await;
            (
                name,
                result.map(|transport| ConnectedServer { transport, config }),
            )
        });
    }

    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok((name, Ok(connected))) => {
                servers.insert(name, connected);
            }
            Ok((name, Err(err))) => {
                eprintln!("MCP server `{name}` failed to start: {err}");
                errors.insert(name, err);
            }
            Err(join_err) => {
                eprintln!("mcp connect join error: {join_err}");
            }
        }
    }

    (servers, errors)
}

async fn connect_one(config: &McpServerConfig) -> Result<Transport> {
    let startup = config.startup_timeout();
    let tool = config.tool_timeout();
    match &config.transport {
        McpServerTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let transport =
                StdioTransport::connect(command, args, env, cwd.as_deref(), startup, tool).await?;
            Ok(Transport::Stdio(transport))
        }
        McpServerTransport::StreamableHttp {
            url,
            bearer_token,
            headers,
        } => {
            let transport = StreamableHttpTransport::connect(
                url,
                bearer_token.clone(),
                headers.clone(),
                startup,
                tool,
            )
            .await?;
            Ok(Transport::Http(transport))
        }
    }
}

/// SYNC seam over the async transports. See the module-level "Sync-over-async
/// bridge" note: we `block_on` the manager's OWN dedicated runtime, panic-safe
/// from any caller context (including the synchronous `McpTool::run`).
impl McpClient for McpConnectionManager {
    fn call_tool(&self, server: &str, tool: &str, args: Option<Value>) -> Result<McpCallResult> {
        let runtime = self.runtime.clone();
        runtime.block_on(self.call_tool_async(server, tool, args))
    }
}
