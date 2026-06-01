//! `spawn_agent` tool args + spec + spawn flow (codex
//! `tools/handlers/multi_agents_v2/spawn.rs` + `multi_agents_spec.rs` parity).
//!
//! Parity:
//! - Args struct: `core/src/tools/handlers/multi_agents_v2/spawn.rs:242-289`
//!   `SpawnAgentArgs { message, task_name, agent_type, model, reasoning_effort,
//!   service_tier, fork_turns }` (+ `deny_unknown_fields`).
//! - `fork_turns` parse: same file `:256-289` — trimmed, default `"all"`;
//!   `"none"` → no fork, `"all"` → full history, else a positive integer.
//! - Tool spec: `core/src/tools/handlers/multi_agents_spec.rs:75-109`
//!   `create_spawn_agent_tool_v2` → name `"spawn_agent"`, required
//!   `["task_name","message"]`; properties `:584-621`.
//! - Depth: `core/src/agent/registry.rs:71-77` + enforcement at
//!   `multi_agents_common.rs:284`.

use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use super::depth::exceeds_depth_limit;
use super::depth::next_spawn_depth;

/// The `fork_turns` semantics (codex `SpawnAgentForkMode` projection).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkTurns {
    /// `"none"` — do not fork the parent's history into the child.
    None,
    /// `"all"` — fork the parent's full history (codex `FullHistory`).
    All,
    /// A numeric string `"N"` (N >= 1) — fork the last N turns
    /// (codex `LastNTurns`).
    N(u32),
}

impl Default for ForkTurns {
    fn default() -> Self {
        // Codex default when `fork_turns` is omitted/empty is "all".
        ForkTurns::All
    }
}

impl ForkTurns {
    /// Parse the `fork_turns` string (codex `spawn.rs:256-289`).
    ///
    /// Trimmed, case-insensitive for the keywords; empty/absent => [`All`].
    /// A numeric value must be a positive integer (`0` is rejected).
    pub fn parse(raw: Option<&str>) -> Result<ForkTurns, String> {
        let value = raw
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("all");
        if value.eq_ignore_ascii_case("none") {
            return Ok(ForkTurns::None);
        }
        if value.eq_ignore_ascii_case("all") {
            return Ok(ForkTurns::All);
        }
        let n = value.parse::<u32>().map_err(|_| {
            "fork_turns must be `none`, `all`, or a positive integer string".to_string()
        })?;
        if n == 0 {
            return Err(
                "fork_turns must be `none`, `all`, or a positive integer string".to_string(),
            );
        }
        Ok(ForkTurns::N(n))
    }
}

/// Deserialized `spawn_agent` arguments (codex `SpawnAgentArgs` :242-253).
///
/// `deny_unknown_fields` matches codex so a model that hallucinates an extra
/// property is rejected. `message` + `task_name` are required by virtue of being
/// non-`Option`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpawnAgentArgs {
    pub message: String,
    pub task_name: String,
    #[serde(default, skip)]
    pub input_items: Option<Value>,
    #[serde(default, skip)]
    pub input_is_inter_agent_communication: bool,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub fork_turns: Option<String>,
    #[serde(default)]
    pub fork_context: Option<bool>,
}

impl SpawnAgentArgs {
    /// Parse from a JSON value (the shape a model emits as tool arguments).
    pub fn from_value(value: Value) -> Result<Self, String> {
        serde_json::from_value(value).map_err(|e| format!("invalid spawn_agent arguments: {e}"))
    }

    /// Resolve `fork_turns` to a [`ForkTurns`].
    pub fn fork_turns_mode(&self) -> Result<ForkTurns, String> {
        if self.fork_context.is_some() {
            return Err(
                "fork_context is not supported in MultiAgentV2; use fork_turns instead".to_string(),
            );
        }
        ForkTurns::parse(self.fork_turns.as_deref())
    }

    /// The role name a caller requested, trimmed and non-empty (codex
    /// `spawn.rs:58-62`). `None` => the default role.
    pub fn role_name(&self) -> Option<&str> {
        self.agent_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// Validate `task_name`: lowercase letters, digits, and underscores only
    /// (codex `multi_agents_spec.rs` `task_name` property description). Empty is
    /// rejected.
    pub fn validate_task_name(&self) -> Result<(), String> {
        validate_task_name(&self.task_name)
    }

    pub fn validate_overrides(&self) -> Result<(), String> {
        if let Some(model) = self.model.as_deref() {
            if model.trim().is_empty() {
                return Err("model override must not be empty".to_string());
            }
        }
        if let Some(service_tier) = self.service_tier.as_deref() {
            if service_tier.trim().is_empty() {
                return Err("service_tier override must not be empty".to_string());
            }
        }
        if let Some(reasoning) = self.reasoning_effort.as_deref() {
            let normalized = reasoning.trim().to_ascii_lowercase().replace('-', "_");
            if !matches!(
                normalized.as_str(),
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
            ) {
                return Err(format!(
                    "reasoning_effort must be one of none, minimal, low, medium, high, or xhigh; got `{reasoning}`"
                ));
            }
        }
        Ok(())
    }
}

/// `task_name` must be lowercase ascii letters, digits, or `_`, and non-empty
/// (codex parity).
pub fn validate_task_name(task_name: &str) -> Result<(), String> {
    if task_name.is_empty() {
        return Err("task_name must not be empty".to_string());
    }
    if matches!(task_name, "root" | "." | "..") {
        return Err(format!("task_name `{task_name}` is reserved"));
    }
    let ok = task_name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "task_name '{task_name}' must contain only lowercase letters, digits, and underscores"
        ))
    }
}

/// The error returned when a spawn would exceed the depth limit (codex
/// rejects in the handler at `multi_agents_common.rs:284`).
pub fn depth_limit_error(child_depth: i32, max_depth: i32) -> String {
    format!(
        "cannot spawn agent: spawn depth {child_depth} exceeds the maximum allowed depth \
         {max_depth}"
    )
}

/// Pre-flight a spawn request against the depth limit. Returns the computed
/// child depth on success (codex: child depth = `next_thread_spawn_depth`,
/// rejected if `exceeds_thread_spawn_depth_limit`).
pub fn check_spawn_depth(parent_depth: i32, max_depth: i32) -> Result<i32, String> {
    let child_depth = next_spawn_depth(parent_depth);
    if exceeds_depth_limit(child_depth, max_depth) {
        return Err(depth_limit_error(child_depth, max_depth));
    }
    Ok(child_depth)
}

/// The tool name (codex `multi_agents_spec.rs:75-109`).
pub const SPAWN_AGENT_TOOL_NAME: &str = "spawn_agent";

/// Build the `spawn_agent` tool spec as a JSON object (codex
/// `create_spawn_agent_tool_v2` → name `"spawn_agent"`, required
/// `["task_name","message"]`, properties at `:584-621`).
///
/// Emitting JSON keeps this module's parser tests independent from the runtime
/// tool registry; production registration uses the definitions in
/// [`crate::tools::registry::definitions`].
pub fn spawn_agent_tool_spec() -> Value {
    json!({
        "name": SPAWN_AGENT_TOOL_NAME,
        "description": "Spawn a sub-agent to work on a delegated task.",
        "parameters": {
            "type": "object",
            "additionalProperties": false,
            "required": ["task_name", "message"],
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The task/message for the new agent."
                },
                "task_name": {
                    "type": "string",
                    "description": "Short canonical name for the task (lowercase letters, digits, underscores)."
                },
                "agent_type": {
                    "type": "string",
                    "description": "Optional role for the new agent. If omitted, `default` is used."
                },
                "fork_turns": {
                    "type": "string",
                    "description": "`none`, `all`, or a positive integer. Defaults to `all`."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for the new agent."
                },
                "reasoning_effort": {
                    "type": "string",
                    "description": "Optional reasoning-effort override for the new agent."
                },
                "service_tier": {
                    "type": "string",
                    "description": "Optional service-tier override for the new agent."
                }
            }
        }
    })
}
