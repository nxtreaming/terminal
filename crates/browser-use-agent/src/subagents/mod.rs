//! `subagents/` — SUBAGENTS subsystem (codex multi-agents-v2 parity).
//!
//! Roles-as-a-config-layer, spawn-depth limits, and an EVENT-NOTIFY mailbox,
//! tying together a live-agent registry and a `ChildSpawner` seam.
//!
//! - [`role`]     — `AgentRoleConfig` + `RoleRegistry` + `apply_role_to_config`
//!   (built-in `default`/`explorer`/`worker`, user overrides win, provider/tier
//!   preserved). codex `agent/role.rs`.
//! - [`depth`]    — `next_spawn_depth` / `exceeds_depth_limit` /
//!   `DEFAULT_AGENT_MAX_DEPTH`. codex `agent/registry.rs:71-77`.
//! - [`mailbox`]  — the `watch`-backed EVENT-NOTIFY mailbox +
//!   `SubagentNotification`. codex `session/input_queue.rs`.
//! - [`registry`] — live-agent tracking + `<subagents>` env block. codex
//!   `agent/registry.rs` + legacy env-context.
//! - [`spawn`]    — `SpawnAgentArgs` + `ForkTurns` + the `spawn_agent` tool spec
//!   + depth pre-flight. codex `multi_agents_v2/spawn.rs` + `multi_agents_spec`.
//! - [`manager`]  — `SubagentManager` + the `ChildSpawner` trait seam + budget
//!   accounting.

pub mod depth;
pub mod mailbox;
pub mod manager;
pub mod registry;
pub mod role;
pub mod spawn;

pub use depth::{exceeds_depth_limit, next_spawn_depth, DEFAULT_AGENT_MAX_DEPTH};
pub use mailbox::{AgentStatus, InterAgentCommunication, Mailbox, SubagentNotification};
pub use manager::{
    ChildHandle, ChildSpawner, ChildSpec, ParentContext, SubagentError, SubagentManager,
};
pub use registry::{AgentRecord, AgentRegistry};
pub use role::{
    built_in_roles, AgentConfigLayer, AgentRoleConfig, RoleOverrides, RoleRegistry,
    DEFAULT_ROLE_NAME,
};
pub use spawn::{
    check_spawn_depth, spawn_agent_tool_spec, validate_task_name, ForkTurns, SpawnAgentArgs,
    SPAWN_AGENT_TOOL_NAME,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
