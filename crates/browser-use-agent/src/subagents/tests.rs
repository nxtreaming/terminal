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
async fn v2_spawn_allows_depth_past_configured_max() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    // Codex MultiAgentV2 records depth but does not disable nested v2 spawning at
    // `agent_max_depth`; only older collaboration tools are disabled there.
    let parent = parent_ctx("/root/worker", DEFAULT_AGENT_MAX_DEPTH);
    let handle = manager
        .spawn(spawn_args("dig_deeper", "go"), &parent)
        .await
        .expect("v2 spawn should ignore the configured max depth");
    assert_eq!(handle.agent_path, "/root/worker/dig_deeper");
    assert_eq!(manager.registry().get(&handle.agent_path).unwrap().depth, 2);
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
    assert_eq!(handle.agent_path, "/root/explore_db");
    assert_eq!(manager.list_agents().len(), 2);
}

#[tokio::test]
async fn v2_concurrent_thread_cap_counts_root_like_codex() {
    let manager = fake_manager_with_limit(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH, Some(2));
    let parent = parent_ctx("/root", 0);
    manager
        .spawn(spawn_args("first", "ok"), &parent)
        .await
        .expect("cap 2 allows root plus one spawned agent");
    let err = manager
        .spawn(spawn_args("second", "no room"), &parent)
        .await
        .expect_err("second child exceeds cap 2 because root consumes one slot");
    assert!(err.to_string().contains("agent limit reached"));

    let manager = fake_manager_with_limit(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH, Some(3));
    manager
        .spawn(spawn_args("first", "ok"), &parent)
        .await
        .expect("first child fits under cap 3");
    manager
        .spawn(spawn_args("second", "ok"), &parent)
        .await
        .expect("second child fits under cap 3");
    let err = manager
        .spawn(spawn_args("third", "no room"), &parent)
        .await
        .expect_err("third child exceeds cap 3 because root consumes one slot");
    assert!(err.to_string().contains("agent limit reached"));
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

    // Codex's built-in explorer currently points at an empty built-in role file,
    // so it marks the role without changing the inherited child config.
    assert!(config.instructions.is_empty());
    assert!(config.can_write);
    assert_eq!(config.role.as_deref(), Some("explorer"));
    // Caller's provider/tier preserved (explorer role sets neither).
    assert_eq!(config.provider, "parent-provider");
    assert_eq!(config.service_tier.as_deref(), Some("parent-tier"));
    assert_eq!(
        role.config_file.as_deref(),
        Some(std::path::Path::new("explorer.toml"))
    );
    assert!(role.nickname_candidates.is_none());
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
    // Pool exhausted -> reset with an ordinal suffix like Codex.
    let reset = registry.reserve_nickname(&pool).expect("reset nickname");
    assert!(reset.ends_with(" the 2nd"));
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
    let note = SubagentNotification::new("/root/explorer_1", AgentStatus::Completed(None));
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
    assert_eq!(status, AgentStatus::Completed(None));
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
    assert_eq!(agents.len(), 2);
    assert!(
        agents
            .iter()
            .any(|agent| agent.agent_path == handle.agent_path),
        "spawned child should be listed: {agents:?}"
    );

    assert!(manager.close_agent(&handle.agent_path));
    let after = manager.registry().get(&handle.agent_path).expect("record");
    assert_eq!(after.status, AgentStatus::Shutdown);
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
        last_task_message: Some("inspect repo".to_string()),
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

#[tokio::test]
async fn spawn_rejects_duplicate_task_path() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    let parent = parent_ctx("/root", 0);
    manager
        .spawn(spawn_args("explore", "first"), &parent)
        .await
        .expect("first spawn");
    let err = manager
        .spawn(spawn_args("explore", "second"), &parent)
        .await
        .expect_err("second spawn must collide");
    assert!(err.0.contains("already exists"), "unexpected error: {err}");
}

#[tokio::test]
async fn send_message_resolves_relative_target_and_updates_last_task() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    let parent = parent_ctx("/root", 0);
    let handle = manager
        .spawn(spawn_args("worker_one", "do work"), &parent)
        .await
        .expect("spawn");
    manager.mailbox().drain();

    let target = manager
        .send_message_to_agent(&parent, "worker_one", "more context", false)
        .expect("message target resolves");
    assert_eq!(target.agent_path, handle.agent_path);
    let updated = manager.registry().get(&handle.agent_path).unwrap();
    assert_eq!(updated.last_task_message.as_deref(), Some("more context"));
    let drained = manager.mailbox().drain();
    assert_eq!(drained.len(), 1);
    assert!(!drained[0].trigger_turn);
}

#[tokio::test]
async fn close_agent_reference_closes_subtree() {
    let manager = fake_manager(RoleRegistry::new(), 2);
    let parent = parent_ctx("/root", 0);
    manager
        .spawn(spawn_args("worker_one", "do work"), &parent)
        .await
        .expect("spawn");

    let previous = manager
        .close_agent_reference(&parent, "worker_one")
        .expect("close target");
    assert_eq!(previous, AgentStatus::Completed(None));
    assert_eq!(
        manager.registry().get("/root/worker_one").unwrap().status,
        AgentStatus::Shutdown
    );
    assert!(manager.close_agent_reference(&parent, "root").is_err());
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
        "fork_turns": "all"
    });
    let args = SpawnAgentArgs::from_value(value).expect("valid args");
    assert_eq!(args.message, "do the thing");
    assert_eq!(args.task_name, "do_thing");
    assert_eq!(args.role_name(), Some("worker"));
    assert_eq!(args.fork_turns_mode().unwrap(), ForkTurns::All);
}

#[test]
fn deserialize_rejects_non_codex_v2_service_tier() {
    let value = serde_json::json!({
        "message": "do the thing",
        "task_name": "do_thing",
        "service_tier": "priority"
    });

    let err = SpawnAgentArgs::from_value(value).expect_err("service_tier is not a v2 argument");
    assert!(
        err.contains("unknown field") && err.contains("service_tier"),
        "unexpected error: {err}"
    );
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

#[tokio::test]
async fn full_history_spawn_metadata_overrides_become_non_forked_role_spawn() {
    let spawner = Arc::new(RecordingSpawner::default());
    let manager = SubagentManager::with_config(
        spawner.clone(),
        RoleRegistry::new(),
        DEFAULT_AGENT_MAX_DEPTH,
    );
    let parent = parent_ctx("/root", 0);
    let handle = manager
        .spawn(
            SpawnAgentArgs::from_value(serde_json::json!({
                "message": "do it",
                "task_name": "worker",
                "agent_type": "explorer",
                "fork_turns": "all"
            }))
            .unwrap(),
            &parent,
        )
        .await
        .expect("metadata override should normalize instead of failing");
    assert_eq!(handle.agent_path, "/root/worker");
    let specs = spawner.specs.lock().unwrap();
    let spec = specs.last().expect("recorded spec");
    assert_eq!(spec.fork_turns.as_deref(), Some("none"));
    assert_eq!(spec.config.role.as_deref(), Some("explorer"));
}

#[tokio::test]
async fn omitted_fork_turns_with_agent_type_normalizes_instead_of_failing() {
    let spawner = Arc::new(RecordingSpawner::default());
    let manager = SubagentManager::with_config(
        spawner.clone(),
        RoleRegistry::new(),
        DEFAULT_AGENT_MAX_DEPTH,
    );
    let parent = parent_ctx("/root", 0);
    let args = SpawnAgentArgs::from_value(serde_json::json!({
        "message": "inspect disk usage",
        "task_name": "home_large_dirs",
        "agent_type": "explorer"
    }))
    .expect("valid args");

    let handle = manager
        .spawn(args, &parent)
        .await
        .expect("omitted fork_turns with agent_type should normalize instead of failing");
    assert_eq!(handle.agent_path, "/root/home_large_dirs");
    let specs = spawner.specs.lock().unwrap();
    let spec = specs.last().expect("recorded spec");
    assert_eq!(spec.fork_turns.as_deref(), Some("none"));
    assert_eq!(spec.config.role.as_deref(), Some("explorer"));
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

    let reserved = SpawnAgentArgs::from_value(serde_json::json!({
        "message": "m",
        "task_name": "root"
    }))
    .unwrap();
    assert!(reserved.validate_task_name().is_err());

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
    assert!(spec["parameters"]["properties"]["service_tier"].is_null());
}

#[tokio::test]
async fn spawn_rejects_invalid_overrides() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    let parent = parent_ctx("/root", 0);
    let mut args = spawn_args("bad_reasoning", "go");
    args.reasoning_effort = Some("super_high".to_string());
    let err = manager
        .spawn(args, &parent)
        .await
        .expect_err("invalid reasoning effort must be rejected");
    assert!(err.to_string().contains("reasoning_effort must be one of"));

    let mut args = spawn_args("empty_model", "go");
    args.model = Some("  ".to_string());
    let err = manager
        .spawn(args, &parent)
        .await
        .expect_err("empty model override must be rejected");
    assert!(err.to_string().contains("model override must not be empty"));
}

#[tokio::test]
async fn spawn_validates_model_reasoning_and_internal_service_tier() {
    let spawner = Arc::new(RecordingSpawner::default());
    let manager = SubagentManager::with_config(
        spawner.clone(),
        RoleRegistry::new(),
        DEFAULT_AGENT_MAX_DEPTH,
    );
    let parent = parent_ctx("/root", 0);
    let mut args = spawn_args("catalog_model", "go");
    args.model = Some("gpt-5.5".to_string());
    args.reasoning_effort = Some("high".to_string());
    args.service_tier = Some("priority".to_string());

    manager.spawn(args, &parent).await.expect("spawn ok");

    let specs = spawner.specs.lock().unwrap();
    let spec = specs.last().expect("recorded spec");
    assert_eq!(spec.config.model, "gpt-5.5");
    assert_eq!(spec.config.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(spec.config.service_tier.as_deref(), Some("priority"));
}

#[tokio::test]
async fn spawn_rejects_unknown_model_and_unsupported_reasoning_or_tier() {
    let manager = fake_manager(RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH);
    let parent = parent_ctx("/root", 0);

    let mut args = spawn_args("unknown_model", "go");
    args.model = Some("not-a-codex-model".to_string());
    let err = manager
        .spawn(args, &parent)
        .await
        .expect_err("unknown model must be rejected");
    assert!(err.0.contains("Unknown model `not-a-codex-model`"));

    let mut args = spawn_args("bad_reasoning_model", "go");
    args.model = Some("gpt-5.5".to_string());
    args.reasoning_effort = Some("minimal".to_string());
    let err = manager
        .spawn(args, &parent)
        .await
        .expect_err("unsupported reasoning must be rejected");
    assert!(err
        .0
        .contains("Reasoning effort `minimal` is not supported"));

    let mut args = spawn_args("bad_tier_model", "go");
    args.model = Some("gpt-5.3-codex".to_string());
    args.service_tier = Some("priority".to_string());
    let err = manager
        .spawn(args, &parent)
        .await
        .expect_err("unsupported service tier must be rejected");
    assert!(err.0.contains("Service tier `priority` is not supported"));
}

#[tokio::test]
async fn spawn_preserves_parent_service_tier_when_model_supports_it() {
    let spawner = Arc::new(RecordingSpawner::default());
    let manager = SubagentManager::with_config(
        spawner.clone(),
        RoleRegistry::new(),
        DEFAULT_AGENT_MAX_DEPTH,
    );
    let mut parent = parent_ctx("/root", 0);
    parent.base_config.model = "gpt-5.5".to_string();
    parent.base_config.service_tier = Some("priority".to_string());

    manager
        .spawn(spawn_args("inherit_tier", "go"), &parent)
        .await
        .expect("spawn ok");

    let specs = spawner.specs.lock().unwrap();
    let spec = specs.last().expect("recorded spec");
    assert_eq!(spec.config.service_tier.as_deref(), Some("priority"));
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
        "agent_type": "explorer",
        "fork_turns": "none"
    }))
    .expect("valid args")
}

fn fake_manager(roles: RoleRegistry, max_depth: i32) -> SubagentManager {
    fake_manager_with_limit(roles, max_depth, None)
}

fn fake_manager_with_limit(
    roles: RoleRegistry,
    max_depth: i32,
    max_concurrent_threads_per_session: Option<usize>,
) -> SubagentManager {
    // Build the manager first so the fake spawner can share its mailbox +
    // registry to simulate a child reporting completion.
    let spawner = Arc::new(FakeSpawner::new());
    let manager = SubagentManager::with_config_and_limits(
        spawner.clone(),
        roles,
        max_depth,
        max_concurrent_threads_per_session,
    );
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
                .update_status(&spec.agent_path, AgentStatus::Completed(None));
            // The child reports its completion through the mailbox (the wake).
            let note = SubagentNotification::new(&spec.agent_path, AgentStatus::Completed(None));
            shared.mailbox.enqueue(InterAgentCommunication::new(
                spec.agent_path.clone(),
                "/root",
                Vec::new(),
                note.render(),
                /*trigger_turn*/ false,
            ));
        }
        Ok(ChildHandle {
            agent_path: spec.agent_path,
            agent_id: spec.agent_id,
        })
    }
}

#[derive(Default)]
struct RecordingSpawner {
    specs: Mutex<Vec<ChildSpec>>,
}

#[async_trait::async_trait]
impl ChildSpawner for RecordingSpawner {
    async fn spawn_child(&self, spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        let handle = ChildHandle {
            agent_path: spec.agent_path.clone(),
            agent_id: spec.agent_id.clone(),
        };
        self.specs.lock().unwrap().push(spec);
        Ok(handle)
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
