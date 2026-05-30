//! Network-free tests for the SUBAGENTS subsystem.
//!
//! No real model, no real turn loop: spawning goes through a fake
//! [`ChildSpawner`] that registers a canned child and enqueues a completion
//! notification onto the shared mailbox.

use std::sync::Arc;
use std::sync::Mutex;

use tokio::time::Duration;
use tokio::time::Instant;

use super::depth::exceeds_depth_limit;
use super::depth::next_spawn_depth;
use super::depth::DEFAULT_AGENT_MAX_DEPTH;
use super::mailbox::AgentStatus;
use super::mailbox::InterAgentCommunication;
use super::mailbox::Mailbox;
use super::mailbox::SubagentNotification;
use super::manager::ChildHandle;
use super::manager::ChildSpawner;
use super::manager::ChildSpec;
use super::manager::ParentContext;
use super::manager::SubagentError;
use super::manager::SubagentManager;
use super::registry::AgentRecord;
use super::registry::AgentRegistry;
use super::role::AgentConfigLayer;
use super::role::AgentRoleConfig;
use super::role::RoleOverrides;
use super::role::RoleRegistry;
use super::spawn::spawn_agent_tool_spec;
use super::spawn::ForkTurns;
use super::spawn::SpawnAgentArgs;

// ---------------------------------------------------------------------------
// depth
// ---------------------------------------------------------------------------

#[test]
fn depth_computation_and_limit() {
    assert_eq!(next_spawn_depth(0), 1);
    assert_eq!(next_spawn_depth(1), 2);
    // At max (1) the child depth 1 is allowed; depth 2 is not.
    assert!(!exceeds_depth_limit(1, DEFAULT_AGENT_MAX_DEPTH));
    assert!(exceeds_depth_limit(2, DEFAULT_AGENT_MAX_DEPTH));
}

#[tokio::test]
async fn spawn_rejected_at_depth_equal_to_max() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    // Parent already at depth == max (1): child would be depth 2 > 1 -> reject.
    let parent = parent_ctx("/root/worker", DEFAULT_AGENT_MAX_DEPTH);
    let err = manager
        .spawn(spawn_args("dig_deeper", "go"), &parent)
        .await
        .expect_err("spawn at max depth must be rejected");
    assert!(err.0.contains("exceeds"), "unexpected error: {}", err.0);
    // Nothing registered.
    assert!(manager.list_agents().is_empty());
}

#[tokio::test]
async fn spawn_succeeds_below_depth_limit() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    // Parent at depth 0: child depth 1 <= max 1 -> allowed.
    let parent = parent_ctx("/root", 0);
    let handle = manager
        .spawn(spawn_args("explore_db", "find the db layer"), &parent)
        .await
        .expect("spawn below limit must succeed");
    assert!(handle.agent_path.starts_with("/root/explore_db_"));
    assert_eq!(manager.list_agents().len(), 1);
}

// ---------------------------------------------------------------------------
// role
// ---------------------------------------------------------------------------

#[test]
fn builtin_roles_resolve() {
    let registry = RoleRegistry::new();
    assert!(registry.resolve("default").is_some());
    assert!(registry.resolve("explorer").is_some());
    assert!(registry.resolve("worker").is_some());
    assert!(registry.resolve("nope").is_none());
}

#[test]
fn user_defined_role_overrides_builtin_of_same_name() {
    let mut registry = RoleRegistry::new();
    let custom = AgentRoleConfig {
        description: Some("custom worker".to_string()),
        config_file: None,
        nickname_candidates: Some(vec!["Zed".to_string()]),
        overrides: RoleOverrides {
            instructions: Some("custom worker instructions".to_string()),
            ..RoleOverrides::default()
        },
    };
    registry.register_user_role("worker", custom.clone());

    let resolved = registry.resolve("worker").expect("worker resolves");
    assert_eq!(resolved.description.as_deref(), Some("custom worker"));
    assert_eq!(
        resolved.overrides.instructions.as_deref(),
        Some("custom worker instructions")
    );
    assert!(registry.is_user_defined("worker"));
    assert!(!registry.is_user_defined("explorer"));
}

#[test]
fn apply_role_layers_fields_but_preserves_caller_provider_and_tier() {
    let registry = RoleRegistry::new();
    let mut config = AgentConfigLayer::base("parent-model", "parent-provider");
    config.service_tier = Some("parent-tier".to_string());

    let role = registry
        .apply_role_to_config(&mut config, Some("explorer"))
        .expect("explorer applies");

    // Role set instructions + can_write (explorer is read-only).
    assert!(config.instructions.contains("explorer"));
    assert_eq!(config.can_write, false);
    assert_eq!(config.role.as_deref(), Some("explorer"));
    // Caller's provider/tier preserved (explorer role sets neither).
    assert_eq!(config.provider, "parent-provider");
    assert_eq!(config.service_tier.as_deref(), Some("parent-tier"));
    // Nickname pool available on the role.
    assert!(role.nickname_candidates.is_some());
}

#[test]
fn apply_role_overrides_provider_and_tier_when_role_sets_them() {
    let mut registry = RoleRegistry::new();
    registry.register_user_role(
        "locked",
        AgentRoleConfig {
            description: Some("locked".to_string()),
            config_file: None,
            nickname_candidates: None,
            overrides: RoleOverrides {
                provider: Some("role-provider".to_string()),
                service_tier: Some("role-tier".to_string()),
                model: Some("role-model".to_string()),
                ..RoleOverrides::default()
            },
        },
    );
    let mut config = AgentConfigLayer::base("parent-model", "parent-provider");
    config.service_tier = Some("parent-tier".to_string());

    registry
        .apply_role_to_config(&mut config, Some("locked"))
        .expect("locked applies");

    assert_eq!(config.provider, "role-provider");
    assert_eq!(config.service_tier.as_deref(), Some("role-tier"));
    assert_eq!(config.model, "role-model");
}

#[test]
fn nickname_drawn_from_candidates_without_collision() {
    let registry = AgentRegistry::new();
    let pool = vec!["Ada".to_string(), "Lin".to_string()];
    let first = registry.reserve_nickname(&pool).expect("first nickname");
    let second = registry.reserve_nickname(&pool).expect("second nickname");
    assert_ne!(first, second);
    assert!(pool.contains(&first) && pool.contains(&second));
    // Pool exhausted -> None.
    assert!(registry.reserve_nickname(&pool).is_none());
}

// ---------------------------------------------------------------------------
// mailbox event-notify
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_change_future_is_pending_until_enqueue() {
    let mailbox = Mailbox::new();
    let mut rx = mailbox.subscribe();

    // Before any enqueue, `changed()` must NOT be ready: poll it once with a
    // zero deadline and confirm it times out (proves no busy-poll completion).
    let immediate = Mailbox::wait_for_change(&mut rx, Instant::now()).await;
    assert!(
        !immediate,
        "changed() resolved before any enqueue (would imply a poll/spurious wake)"
    );

    // Now enqueue from a separate task; the waiter wakes only after that.
    let comm = InterAgentCommunication::new("/root/worker", "/root", Vec::new(), "done", false);
    mailbox.enqueue(comm);

    let woken = Mailbox::wait_for_change(&mut rx, Instant::now() + Duration::from_secs(5)).await;
    assert!(woken, "changed() must resolve after enqueue");
}

#[test]
fn mailbox_drains_fifo() {
    let mailbox = Mailbox::new();
    let one = InterAgentCommunication::new("/root", "/root/w", Vec::new(), "one", false);
    let two = InterAgentCommunication::new("/root", "/root/w", Vec::new(), "two", false);
    mailbox.enqueue(one.clone());
    mailbox.enqueue(two.clone());

    let drained = mailbox.drain();
    assert_eq!(drained, vec![one, two]);
    assert!(!mailbox.has_pending());
}

#[test]
fn subagent_notification_render_contains_markers_and_status() {
    let note = SubagentNotification::new("/root/explorer_1", AgentStatus::Completed);
    let rendered = note.render();
    assert!(rendered.contains("<subagent_notification>"));
    assert!(rendered.contains("</subagent_notification>"));
    assert!(rendered.contains("completed"));
    assert!(rendered.contains("/root/explorer_1"));
}

// ---------------------------------------------------------------------------
// spawn + wait roundtrip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_then_wait_observes_completion_via_mailbox() {
    let manager = Arc::new(fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH));
    let parent = parent_ctx("/root", 0);

    let handle = manager
        .spawn(spawn_args("explore", "go find it"), &parent)
        .await
        .expect("spawn succeeds");

    // The fake spawner already enqueued a completion notification + flipped the
    // registry status to Completed (see FakeSpawner). The parent waits and wakes
    // via the mailbox, NOT a poll.
    let status = manager
        .wait(&handle.agent_path, Duration::from_secs(5))
        .await
        .expect("wait wakes via mailbox before timeout");
    assert_eq!(status, AgentStatus::Completed);
}

#[tokio::test]
async fn wait_times_out_when_no_mailbox_activity() {
    // Spawner that does NOT enqueue anything.
    let spawner = Arc::new(SilentSpawner);
    let manager =
        SubagentManager::with_config(spawner, RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    let parent = parent_ctx("/root", 0);
    let handle = manager
        .spawn(spawn_args("quiet", "no news"), &parent)
        .await
        .expect("spawn succeeds");

    let result = manager
        .wait(&handle.agent_path, Duration::from_millis(20))
        .await;
    assert!(result.is_none(), "wait must time out when no mail arrives");
}

// ---------------------------------------------------------------------------
// registry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_reflects_spawn_and_close() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    let parent = parent_ctx("/root", 0);
    let handle = manager
        .spawn(spawn_args("worker_one", "do work"), &parent)
        .await
        .expect("spawn");

    let agents = manager.list_agents();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].agent_path, handle.agent_path);

    assert!(manager.close_agent(&handle.agent_path));
    let after = manager.registry().get(&handle.agent_path).expect("record");
    assert_eq!(after.status, AgentStatus::Closed);
    assert!(!manager.close_agent("/root/does_not_exist"));
}

#[test]
fn subagents_env_block_shape() {
    let registry = AgentRegistry::new();
    assert_eq!(
        registry.render_subagents_block(),
        "<subagents>\n</subagents>"
    );

    registry.register(AgentRecord {
        agent_path: "/root/explore_1".to_string(),
        agent_id: "agent-1".to_string(),
        nickname: Some("Ada".to_string()),
        role: Some("explorer".to_string()),
        status: AgentStatus::Running,
        depth: 1,
    });
    let block = registry.render_subagents_block();
    assert!(block.starts_with("<subagents>\n"));
    assert!(block.ends_with("</subagents>"));
    assert!(block.contains("path=\"/root/explore_1\""));
    assert!(block.contains("nickname=\"Ada\""));
    assert!(block.contains("role=\"explorer\""));
    assert!(block.contains("status=\"running\""));
    assert!(block.contains("depth=\"1\""));
}

// ---------------------------------------------------------------------------
// budget
// ---------------------------------------------------------------------------

#[test]
fn child_usage_aggregates_onto_parent() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    assert_eq!(manager.child_usage_total(), 0);
    manager.account_child_usage(120);
    manager.account_child_usage(80);
    assert_eq!(manager.child_usage_total(), 200);
}

// ---------------------------------------------------------------------------
// spawn_agent args
// ---------------------------------------------------------------------------

#[test]
fn deserialize_model_style_args() {
    let value = serde_json::json!({
        "message": "do the thing",
        "task_name": "do_thing",
        "agent_type": "worker",
        "model": "fast",
        "reasoning_effort": "high",
        "service_tier": "priority",
        "fork_turns": "all"
    });
    let args = SpawnAgentArgs::from_value(value).expect("valid args");
    assert_eq!(args.message, "do the thing");
    assert_eq!(args.task_name, "do_thing");
    assert_eq!(args.role_name(), Some("worker"));
    assert_eq!(args.fork_turns_mode().unwrap(), ForkTurns::All);
}

#[test]
fn fork_turns_parses_none_all_numeric() {
    assert_eq!(ForkTurns::parse(Some("none")).unwrap(), ForkTurns::None);
    assert_eq!(ForkTurns::parse(Some("all")).unwrap(), ForkTurns::All);
    assert_eq!(ForkTurns::parse(Some("3")).unwrap(), ForkTurns::N(3));
    // Default when absent/empty is All (codex parity).
    assert_eq!(ForkTurns::parse(None).unwrap(), ForkTurns::All);
    assert_eq!(ForkTurns::parse(Some("  ")).unwrap(), ForkTurns::All);
    // Zero and non-numeric are rejected.
    assert!(ForkTurns::parse(Some("0")).is_err());
    assert!(ForkTurns::parse(Some("lots")).is_err());
}

#[test]
fn invalid_task_name_rejected() {
    let upper = SpawnAgentArgs::from_value(serde_json::json!({
        "message": "m",
        "task_name": "BadName"
    }))
    .unwrap();
    assert!(upper.validate_task_name().is_err());

    let spaced = SpawnAgentArgs::from_value(serde_json::json!({
        "message": "m",
        "task_name": "bad name"
    }))
    .unwrap();
    assert!(spaced.validate_task_name().is_err());

    let good = SpawnAgentArgs::from_value(serde_json::json!({
        "message": "m",
        "task_name": "good_name_2"
    }))
    .unwrap();
    assert!(good.validate_task_name().is_ok());
}

#[test]
fn missing_required_field_rejected() {
    // No `message`.
    assert!(SpawnAgentArgs::from_value(serde_json::json!({ "task_name": "x" })).is_err());
    // No `task_name`.
    assert!(SpawnAgentArgs::from_value(serde_json::json!({ "message": "x" })).is_err());
    // Unknown field rejected (deny_unknown_fields).
    assert!(SpawnAgentArgs::from_value(serde_json::json!({
        "message": "x",
        "task_name": "y",
        "bogus": true
    }))
    .is_err());
}

#[test]
fn tool_spec_has_codex_name_and_required_params() {
    let spec = spawn_agent_tool_spec();
    assert_eq!(spec["name"], "spawn_agent");
    let required = spec["parameters"]["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "task_name"));
    assert!(required.iter().any(|v| v == "message"));
    // additionalProperties: false (deny extras at the schema level too).
    assert_eq!(spec["parameters"]["additionalProperties"], false);
}

// ---------------------------------------------------------------------------
// helpers / fakes
// ---------------------------------------------------------------------------

fn parent_ctx(agent_path: &str, depth: i32) -> ParentContext {
    ParentContext {
        agent_path: agent_path.to_string(),
        depth,
        base_config: AgentConfigLayer::base("parent-model", "parent-provider"),
    }
}

fn spawn_args(task_name: &str, message: &str) -> SpawnAgentArgs {
    SpawnAgentArgs::from_value(serde_json::json!({
        "message": message,
        "task_name": task_name,
        "agent_type": "explorer"
    }))
    .expect("valid args")
}

fn fake_manager(roles: RoleRegistry, max_depth: i32) -> SubagentManager {
    // Build the manager first so the fake spawner can share its mailbox +
    // registry to simulate a child reporting completion.
    let spawner = Arc::new(FakeSpawner::new());
    let manager = SubagentManager::with_config(spawner.clone(), roles, max_depth);
    spawner.attach(manager.mailbox(), manager.registry());
    manager
}

/// A fake child-spawner that, on `spawn_child`, immediately simulates a finished
/// child: it flips the registry status to `Completed` and enqueues a
/// `SubagentNotification(completed)` onto the shared mailbox so a waiting parent
/// wakes via `rx.changed()`.
struct FakeSpawner {
    shared: Mutex<Option<Shared>>,
}

struct Shared {
    mailbox: Arc<Mailbox>,
    registry: Arc<AgentRegistry>,
}

impl FakeSpawner {
    fn new() -> Self {
        Self {
            shared: Mutex::new(None),
        }
    }

    fn attach(&self, mailbox: Arc<Mailbox>, registry: Arc<AgentRegistry>) {
        *self.shared.lock().unwrap() = Some(Shared { mailbox, registry });
    }
}

#[async_trait::async_trait]
impl ChildSpawner for FakeSpawner {
    async fn spawn_child(&self, spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        if let Some(shared) = self.shared.lock().unwrap().as_ref() {
            // Simulate the child running to completion.
            shared
                .registry
                .update_status(&spec.agent_path, AgentStatus::Completed);
            // The child reports its completion through the mailbox (the wake).
            let note = SubagentNotification::new(&spec.agent_path, AgentStatus::Completed);
            shared.mailbox.enqueue(InterAgentCommunication::new(
                spec.agent_path.clone(),
                "/root",
                Vec::new(),
                note.render(),
                /*trigger_turn*/ true,
            ));
        }
        Ok(ChildHandle {
            agent_path: spec.agent_path,
            agent_id: spec.agent_id,
        })
    }
}

/// A spawner that registers the child but never reports anything (for the
/// timeout test).
struct SilentSpawner;

#[async_trait::async_trait]
impl ChildSpawner for SilentSpawner {
    async fn spawn_child(&self, spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        Ok(ChildHandle {
            agent_path: spec.agent_path,
            agent_id: spec.agent_id,
        })
    }
}
