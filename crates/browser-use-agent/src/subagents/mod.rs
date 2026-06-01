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
//! - [`tree`]     — synchronous agent-tree walk + reference resolution over the
//!   live [`registry::AgentRegistry`] (`collect_agent_tree`,
//!   `resolve_agent_reference_in_tree`, `root_session`,
//!   `canonical_agent_reference`). Legacy `browser-use-core` `lib.rs`.
//! - [`store_tree`] — the durable, `Store`-backed variants of the tree +
//!   agent-status helpers (`collect_agent_tree`, `root_session_id`,
//!   `resolve_agent_reference_in_tree`, `display_agent_path_for_session`,
//!   `local_agent_status_value`, `final_statuses_for_v1_wait`,
//!   `last_task_message_for_agent`, `canonical_agent_path_from_task_name`,
//!   `cleanup_agent_runtime_state_for_agent_subtree`). These thread a `&Store`
//!   (vs. [`tree`]'s in-memory registry) for the tui/cli's 28 durable call
//!   sites. Legacy `browser-use-core` `lib.rs`.
//! - [`parent_link`] — parent/child run linkage: record a child's outcome on the
//!   registry and notify the parent through the [`mailbox::Mailbox`]
//!   (`update_parent_from_child_run`). Legacy `browser-use-core` `lib.rs`.

pub mod depth;
pub mod mailbox;
pub mod manager;
pub mod parent_link;
pub mod registry;
pub mod role;
pub mod spawn;
pub mod store_tree;
pub mod tree;

pub use depth::{exceeds_depth_limit, next_spawn_depth, DEFAULT_AGENT_MAX_DEPTH};
pub use mailbox::{AgentStatus, InterAgentCommunication, Mailbox, SubagentNotification};
pub use manager::{
    ChildHandle, ChildSpawner, ChildSpec, ParentContext, SubagentError, SubagentManager,
};
pub use parent_link::{update_parent_from_child_run, ChildRunOutcome, ParentLinkUpdate};
pub use registry::{AgentRecord, AgentRegistry};
pub use role::{
    built_in_roles, AgentConfigLayer, AgentRoleConfig, RoleOverrides, RoleRegistry,
    DEFAULT_ROLE_NAME,
};
pub use spawn::{
    check_spawn_depth, spawn_agent_tool_spec, validate_task_name, ForkTurns, SpawnAgentArgs,
    SPAWN_AGENT_TOOL_NAME,
};
// Store-backed variants. Several names (`collect_agent_tree`,
// `resolve_agent_reference_in_tree`, `root_session_id`) intentionally mirror the
// registry-based [`tree`] ops but take a `&Store`; they are re-exported under a
// `store_` prefix to avoid colliding with the registry re-exports above while
// still surfacing them from the `subagents` root.
pub use store_tree::{
    canonical_agent_path_from_task_name, cleanup_agent_runtime_state_for_agent_subtree,
    collect_agent_subtree_session_ids, display_agent_path_for_session, final_statuses_for_v1_wait,
    last_task_message_for_agent, local_agent_status_value, session_was_interrupted,
    ResolvedAgentReference,
};
pub use store_tree::{
    collect_agent_tree as store_collect_agent_tree,
    resolve_agent_reference_in_tree as store_resolve_agent_reference_in_tree,
    resolve_agent_reference_in_tree_v2 as store_resolve_agent_reference_in_tree_v2,
    root_session_id as store_root_session_id,
};
pub use tree::{
    canonical_agent_reference, collect_agent_tree, resolve_agent_path_v2,
    resolve_agent_reference_in_tree, resolve_agent_reference_in_tree_v2, root_session,
    AgentTreeNode,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
