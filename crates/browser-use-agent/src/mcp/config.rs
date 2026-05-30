//! MCP server configuration.
//!
//! Mirrors codex `config/src/mcp_types.rs` (transport-tagged enum +
//! startup/tool timeouts + enabled/disabled tool filters) and the legacy
//! in-house `McpServerConfig` (`browser-use-core/src/mcp.rs:22-44`). Timeouts
//! are milliseconds here (matching the legacy `startup_timeout_ms` /
//! `tool_timeout_ms`, `:30-31`) rather than codex's `_sec` floats; same intent.
//!
//! Simplifications vs codex (parity debt, see report): the `Stdio.env_vars`
//! source-from-process indirection is dropped (we carry only an explicit `env`
//! map — the legacy merges `env_vars` and `env` into one map anyway,
//! `browser-use-core/src/mcp.rs:189-200`); and `StreamableHttp` carries a
//! literal `bearer_token`/`headers` instead of codex's
//! `bearer_token_env_var`/`env_http_headers` env-source indirection. The
//! codex/legacy `required`, `supports_parallel_tool_calls`, and connector-id
//! features are also dropped (parallel-safety is derived per-tool from the
//! server's `readOnlyHint`; see `protocol::McpToolInfo::read_only_hint`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::mcp::protocol::HeaderMap;

/// Default startup (handshake) timeout. Matches legacy
/// `DEFAULT_MCP_STARTUP_TIMEOUT_MS` (`browser-use-core/src/mcp.rs:14`).
pub const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
/// Default per-tool-call timeout. Matches legacy `DEFAULT_MCP_TOOL_TIMEOUT_MS`
/// (`browser-use-core/src/mcp.rs:15`).
pub const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;

/// Transport-specific configuration. Tagged by `transport`, mirroring codex
/// `McpServerTransportConfig` (`config/src/mcp_types.rs`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "transport")]
pub enum McpServerTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    StreamableHttp {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_token: Option<String>,
        #[serde(default)]
        headers: HeaderMap,
    },
}

/// A single MCP server configuration. Mirrors codex `McpServerConfig`
/// (`config/src/mcp_types.rs`) flattened over the transport, plus the legacy
/// timeout/filter knobs (`browser-use-core/src/mcp.rs:30-33`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct McpServerConfig {
    #[serde(flatten)]
    pub transport: McpServerTransport,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_ms: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_ms: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
}

impl McpServerConfig {
    pub fn startup_timeout(&self) -> Duration {
        Duration::from_millis(
            self.startup_timeout_ms
                .unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS),
        )
    }

    pub fn tool_timeout(&self) -> Duration {
        Duration::from_millis(self.tool_timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT_MS))
    }

    /// Whether a tool name is exposed given the enabled/disabled filters.
    ///
    /// Mirrors legacy `McpServerConfig::allows_tool`
    /// (`browser-use-core/src/mcp.rs:37-44`): if an `enabled_tools` allow-list is
    /// set, the tool must be in it; AND the tool must not be in `disabled_tools`.
    /// (The legacy applies enabled first then disabled; the outcome is identical
    /// to disabled-wins.)
    pub fn is_tool_allowed(&self, tool: &str) -> bool {
        if let Some(enabled) = &self.enabled_tools {
            if !enabled.iter().any(|t| t == tool) {
                return false;
            }
        }
        if let Some(disabled) = &self.disabled_tools {
            if disabled.iter().any(|t| t == tool) {
                return false;
            }
        }
        true
    }
}
