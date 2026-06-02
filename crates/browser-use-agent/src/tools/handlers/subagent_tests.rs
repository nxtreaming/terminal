//! Tests for the subagent orchestration tool handlers.
//!
//! These drive the handlers through the SAME typed `run` the orchestrator calls,
//! over a real [`SubagentManager`] backed by a fake [`ChildSpawner`] (no live
//! model). They prove: a `spawn_agent` call routes into the manager and returns
//! a handle; `wait_agent` observes the child's completion via the mailbox;
//! `send_input` enqueues a communication; `list_agents` reports the registry; and
//! lifecycle events are emitted. A spawn rejection surfaces as a tool error.
//!
//! The fake-spawner sharing pattern mirrors `subagents/tests.rs`: the manager
//! owns its registry + mailbox, so the fake captures them via `attach(..)` after
//! the manager is constructed (so its `update_status` / `enqueue` land where the
//! manager's `wait` / `list_agents` read).

use std::sync::Arc;
use std::sync::Mutex;

use super::*;
use crate::config_overrides::{ChildAgentRunCompletion, ChildAgentRunRequest, ChildAgentRunner};
use crate::subagents::mailbox::{AgentStatus, InterAgentCommunication, Mailbox};
use crate::subagents::manager::{
    ChildHandle, ChildSpawner, ChildSpec, SubagentError, SubagentManager,
};
use crate::subagents::registry::AgentRegistry;
use crate::subagents::role::{AgentConfigLayer, RoleRegistry};
use crate::subagents::spawn::SpawnAgentArgs;
use crate::subagents::DEFAULT_AGENT_MAX_DEPTH;
use crate::tools::runtime::{SandboxAttempt, ToolCtx};
use crate::tools::sandbox::{SandboxLaunch, SandboxPermissions, SandboxType};
use browser_use_protocol::SessionStatus;
use browser_use_store::Store;

/// A fake child-spawner that simulates a child running to completion: it flips
/// the registry status to `Completed` and enqueues a completion notification onto
/// the shared mailbox so a waiting parent wakes via `rx.changed()`. It captures
/// the manager's registry + mailbox via [`attach`](FakeSpawner::attach) after the
/// manager is built (the manager owns them). Mirrors `subagents/tests.rs`.
struct FakeSpawner {
    shared: Mutex<Option<(Arc<Mailbox>, Arc<AgentRegistry>)>>,
}

impl FakeSpawner {
    fn new() -> Self {
        Self {
            shared: Mutex::new(None),
        }
    }

    fn attach(&self, mailbox: Arc<Mailbox>, registry: Arc<AgentRegistry>) {
        *self.shared.lock().unwrap() = Some((mailbox, registry));
    }
}

#[async_trait]
impl ChildSpawner for FakeSpawner {
    async fn spawn_child(&self, spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        if let Some((mailbox, registry)) = self.shared.lock().unwrap().as_ref() {
            registry.update_status(&spec.agent_path, AgentStatus::Completed(None));
            mailbox.enqueue(InterAgentCommunication::new(
                spec.agent_path.clone(),
                "/root",
                Vec::new(),
                "child completed",
                false,
            ));
        }
        Ok(ChildHandle {
            agent_path: spec.agent_path,
            agent_id: spec.agent_id,
        })
    }
}

/// A spawner that always fails (proves spawn rejection surfaces as a tool error).
struct FailingSpawner;

#[async_trait]
impl ChildSpawner for FailingSpawner {
    async fn spawn_child(&self, _spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        Err(SubagentError("spawner unavailable".to_string()))
    }
}

/// An event sink that captures every emitted event for assertions.
#[derive(Default)]
struct CaptureSink {
    events: Mutex<Vec<PendingEvent>>,
}

impl EventSink for CaptureSink {
    fn emit(&self, ev: PendingEvent) {
        self.events.lock().unwrap().push(ev);
    }
}

impl CaptureSink {
    fn types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }
}

/// Build deps + manager where a fake spawner shares the manager's handles.
fn deps_with_fake() -> (Arc<SubagentManager>, Arc<CaptureSink>, SubagentToolDeps) {
    let spawner = Arc::new(FakeSpawner::new());
    let manager = Arc::new(SubagentManager::with_config(
        spawner.clone(),
        RoleRegistry::new(),
        DEFAULT_AGENT_MAX_DEPTH,
    ));
    spawner.attach(manager.mailbox(), manager.registry());

    let sink = Arc::new(CaptureSink::default());
    let deps = SubagentToolDeps {
        manager: Arc::clone(&manager),
        parent: ParentContext {
            agent_path: "/root".to_string(),
            depth: 0,
            base_config: AgentConfigLayer::base("parent-model", "parent-provider"),
        },
        sink: sink.clone(),
        session_id: "test-session".to_string(),
        store: None,
        child_runner: None,
        cleanup_session_runtime: None,
        spawn_gate: Arc::new(tokio::sync::Mutex::new(())),
        wait_timeouts: Default::default(),
        hide_spawn_agent_metadata: false,
        max_concurrent_threads_per_session: None,
    };
    (manager, sink, deps)
}

fn deps_with_store_tree() -> (
    tempfile::TempDir,
    crate::session::SharedStore,
    String,
    String,
    Arc<CaptureSink>,
    SubagentToolDeps,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    let root = store
        .create_session(None, std::path::Path::new("/tmp"))
        .expect("root session");
    store
        .set_status(&root.id, SessionStatus::Running)
        .expect("root running");
    let child = store
        .create_child_session(
            &root.id,
            std::path::Path::new("/tmp"),
            Some("/root/worker"),
            Some("Atlas"),
            Some("explorer"),
        )
        .expect("child session");
    store
        .set_status(&child.id, SessionStatus::Running)
        .expect("child running");
    let shared_store = Arc::new(Mutex::new(store));
    let spawner = Arc::new(FakeSpawner::new());
    let manager = Arc::new(SubagentManager::with_config(
        spawner.clone(),
        RoleRegistry::new(),
        DEFAULT_AGENT_MAX_DEPTH,
    ));
    spawner.attach(manager.mailbox(), manager.registry());
    let sink = Arc::new(CaptureSink::default());
    let deps = SubagentToolDeps {
        manager,
        parent: ParentContext {
            agent_path: "/root".to_string(),
            depth: 0,
            base_config: AgentConfigLayer::base("parent-model", "parent-provider"),
        },
        sink: sink.clone(),
        session_id: root.id.clone(),
        store: Some(shared_store.clone()),
        child_runner: None,
        cleanup_session_runtime: None,
        spawn_gate: Arc::new(tokio::sync::Mutex::new(())),
        wait_timeouts: Default::default(),
        hide_spawn_agent_metadata: false,
        max_concurrent_threads_per_session: None,
    };
    (dir, shared_store, root.id, child.id, sink, deps)
}

fn seed_child_run_config_marker(store: &crate::session::SharedStore, child_id: &str) {
    store
        .lock()
        .unwrap()
        .append_event(
            child_id,
            "agent.run.started",
            serde_json::json!({
                "run_id": "seed-run",
                "model": "child-model",
                "reasoning_effort": "high",
                "service_tier": "priority",
                "config_overrides": [
                    { "key": "model_provider", "value": "anthropic" },
                    { "key": "features.multi_agent_v2.notify", "value": false }
                ],
            }),
        )
        .expect("seed child run config marker");
}

fn assert_child_run_config_replayed(request: &ChildAgentRunRequest) {
    assert_eq!(request.model.as_deref(), Some("child-model"));
    assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(request.service_tier.as_deref(), Some("priority"));
    assert_eq!(
        request.config_overrides,
        vec![
            (
                "model_provider".to_string(),
                toml::Value::String("anthropic".to_string())
            ),
            (
                "features.multi_agent_v2.notify".to_string(),
                toml::Value::Boolean(false)
            ),
        ]
    );
}

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "c".to_string(),
        tool_name: "subagent".to_string(),
        cwd: std::env::temp_dir(),
        artifact_root: std::env::temp_dir().join("artifacts"),
    }
}

/// Run a handler's typed `run` with a benign sandbox attempt (these handlers
/// ignore the attempt). `SandboxLaunch` has no `Default`, so build it explicitly.
async fn run_handler<T, Req>(tool: &T, req: &Req) -> Result<ExecOutput, ToolError>
where
    T: ToolRuntime<Req, ExecOutput>,
    Req: Send + Sync,
{
    let launch = SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: None,
    };
    let att = SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch: &launch,
        cancel: None,
    };
    tool.run(req, &att, &ctx()).await
}

fn spawn_args(task: &str, msg: &str) -> SpawnAgentArgs {
    SpawnAgentArgs {
        message: msg.to_string(),
        task_name: task.to_string(),
        input_items: None,
        input_is_inter_agent_communication: false,
        agent_type: None,
        model: None,
        reasoning_effort: None,
        service_tier: None,
        fork_turns: None,
        fork_context: None,
    }
}

#[test]
fn legacy_v1_requests_ignore_unknown_fields_like_codex() {
    let thread_id = browser_use_store::new_thread_id();

    serde_json::from_value::<SpawnAgentV1Request>(serde_json::json!({
        "message": "start",
        "unexpected": true,
    }))
    .expect("legacy spawn_agent accepts extra fields");
    serde_json::from_value::<WaitAgentV1Request>(serde_json::json!({
        "targets": [thread_id],
        "unexpected": true,
    }))
    .expect("legacy wait_agent accepts extra fields");
    serde_json::from_value::<SendInputRequest>(serde_json::json!({
        "target": browser_use_store::new_thread_id(),
        "message": "hello",
        "unexpected": true,
    }))
    .expect("legacy send_input accepts extra fields");
    serde_json::from_value::<ResumeAgentRequest>(serde_json::json!({
        "id": browser_use_store::new_thread_id(),
        "unexpected": true,
    }))
    .expect("legacy resume_agent accepts extra fields");
    serde_json::from_value::<CloseAgentV1Request>(serde_json::json!({
        "target": browser_use_store::new_thread_id(),
        "unexpected": true,
    }))
    .expect("legacy close_agent accepts extra fields");

    assert!(
        serde_json::from_value::<CloseAgentRequest>(serde_json::json!({
            "target": "worker",
            "unexpected": true,
        }))
        .is_err(),
        "v2 close_agent remains strict"
    );
}

#[test]
fn legacy_v1_items_accept_codex_user_input_fields() {
    let items = vec![
        serde_json::from_value::<LegacyInputItem>(serde_json::json!({
            "type": "text",
            "text": "see attached",
            "text_elements": [{
                "byte_range": { "start": 0, "end": 3 },
                "placeholder": "see"
            }],
            "unexpected": true,
        }))
        .expect("legacy text item should accept text_elements and ignore unknown fields"),
        serde_json::from_value::<LegacyInputItem>(serde_json::json!({
            "type": "image",
            "image_url": "data:image/png;base64,AAAA",
            "detail": "high",
            "unexpected": true,
        }))
        .expect("legacy image item should accept detail and ignore unknown fields"),
    ];

    let payload = legacy_input_payload(None, Some(&items)).expect("items should parse");
    let input_items = payload.input_items.expect("items should be preserved");
    assert_eq!(input_items[0]["text_elements"][0]["placeholder"], "see");
    assert_eq!(input_items[1]["detail"], "high");
    assert!(payload.preview.contains("see attached"));
}

#[tokio::test]
async fn spawn_routes_into_manager_and_returns_handle() {
    let (manager, sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps);

    let out = run_handler(&spawn, &spawn_args("explore", "do the thing"))
        .await
        .expect("spawn should succeed");
    assert_eq!(out.exit_code, 0);
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["task_name"].as_str(), Some("/root/explore"));
    assert!(body.get("nickname").is_some());

    // The spawn registered the child in the manager's registry.
    assert_eq!(manager.list_agents().len(), 2);
    // A durable spawned event was emitted.
    assert!(
        sink.types().contains(&"subagent.spawned".to_string()),
        "expected a subagent.spawned event, got {:?}",
        sink.types()
    );
    assert!(
        sink.types()
            .contains(&"collab_agent_spawn_begin".to_string()),
        "expected a collab_agent_spawn_begin event, got {:?}",
        sink.types()
    );
    assert!(
        sink.types().contains(&"collab_agent_spawn_end".to_string()),
        "expected a collab_agent_spawn_end event, got {:?}",
        sink.types()
    );
    assert!(
        sink.types().contains(&"agent.spawned".to_string()),
        "expected an agent.spawned event, got {:?}",
        sink.types()
    );
}

#[tokio::test]
async fn spawn_can_hide_metadata_in_tool_output() {
    let (_manager, _sink, mut deps) = deps_with_fake();
    deps.hide_spawn_agent_metadata = true;
    let spawn = SpawnAgentTool::new(deps);

    let out = run_handler(&spawn, &spawn_args("explore", "do the thing"))
        .await
        .expect("spawn should succeed");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["task_name"].as_str(), Some("/root/explore"));
    assert!(body.get("nickname").is_none());
}

#[tokio::test]
async fn store_backed_spawn_does_not_emit_duplicate_legacy_spawn_event() {
    let (_dir, _store, _root_id, _child_id, sink, deps) = deps_with_store_tree();
    let spawn = SpawnAgentTool::new(deps);

    run_handler(&spawn, &spawn_args("auditor", "check parity"))
        .await
        .expect("store-backed spawn should succeed through manager seam");

    let types = sink.types();
    assert!(
        types.contains(&"collab_agent_spawn_begin".to_string())
            && types.contains(&"collab_agent_spawn_end".to_string()),
        "expected collab spawn events, got {types:?}"
    );
    assert!(
        !types.contains(&"agent.spawned".to_string()),
        "store-backed handler must leave durable agent.spawned to the child creation path: {types:?}"
    );
}

#[tokio::test]
async fn store_backed_v2_wait_ignores_manager_mailbox_notifications() {
    let (_dir, _store, _root_id, _child_id, _sink, mut deps) = deps_with_store_tree();
    deps.wait_timeouts = WaitAgentTimeoutOptions {
        default_timeout_ms: 1,
        min_timeout_ms: 0,
        max_timeout_ms: 10,
    };
    deps.manager.mailbox().enqueue(InterAgentCommunication::new(
        "/root/worker",
        "/root",
        Vec::new(),
        "manager-only wake",
        true,
    ));
    let wait = WaitAgentTool::new(deps);

    let out = run_handler(
        &wait,
        &WaitAgentRequest {
            timeout_ms: Some(0),
        },
    )
    .await
    .expect("store-backed v2 wait should return from the durable store path");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["timed_out"].as_bool(), Some(true), "{body}");
}

#[tokio::test]
async fn wait_returns_child_completion_via_mailbox() {
    let (_manager, sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let wait = WaitAgentTool::new(deps);

    run_handler(&spawn, &spawn_args("worker", "work"))
        .await
        .expect("spawn ok");
    let req = WaitAgentRequest {
        timeout_ms: Some(10_000),
    };
    let wait_out = run_handler(&wait, &req).await.expect("wait ok");
    let wbody: serde_json::Value = serde_json::from_str(&wait_out.stdout).unwrap();
    assert_eq!(wbody["message"].as_str(), Some("Wait completed."));
    assert_eq!(wbody["timed_out"].as_bool(), Some(false));

    assert!(
        sink.types().contains(&"subagent.spawned".to_string()),
        "expected a subagent.spawned event, got {:?}",
        sink.types()
    );
}

#[tokio::test]
async fn wait_uses_configured_timeout_bounds() {
    let (_manager, _sink, mut deps) = deps_with_fake();
    deps.wait_timeouts = WaitAgentTimeoutOptions {
        default_timeout_ms: 50,
        min_timeout_ms: 0,
        max_timeout_ms: 50,
    };
    let wait = WaitAgentTool::new(deps);

    let default_timeout = run_handler(&wait, &WaitAgentRequest { timeout_ms: None })
        .await
        .expect("default timeout should be accepted");
    let body: serde_json::Value = serde_json::from_str(&default_timeout.stdout).unwrap();
    assert_eq!(body["timed_out"].as_bool(), Some(true));

    run_handler(
        &wait,
        &WaitAgentRequest {
            timeout_ms: Some(0),
        },
    )
    .await
    .expect("configured minimum should be accepted");

    let too_large = run_handler(
        &wait,
        &WaitAgentRequest {
            timeout_ms: Some(51),
        },
    )
    .await
    .expect_err("timeout above configured maximum must fail");
    assert!(
        format!("{too_large:?}").contains("at most 50"),
        "unexpected error: {too_large:?}"
    );
}

#[tokio::test]
async fn wait_rejects_codex_timeout_bounds() {
    let (_manager, _sink, deps) = deps_with_fake();
    let wait = WaitAgentTool::new(deps);

    let too_small = run_handler(
        &wait,
        &WaitAgentRequest {
            timeout_ms: Some(9_999),
        },
    )
    .await
    .expect_err("timeout below codex v2 minimum must fail");
    assert!(
        format!("{too_small:?}").contains("at least 10000"),
        "unexpected error: {too_small:?}"
    );

    let too_large = run_handler(
        &wait,
        &WaitAgentRequest {
            timeout_ms: Some(3_600_001),
        },
    )
    .await
    .expect_err("timeout above codex v2 maximum must fail");
    assert!(
        format!("{too_large:?}").contains("at most 3600000"),
        "unexpected error: {too_large:?}"
    );
}

#[tokio::test]
async fn send_input_enqueues_and_emits_event() {
    let (manager, sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let send = SendInputTool::new(deps);

    run_handler(&spawn, &spawn_args("worker", "work"))
        .await
        .expect("spawn ok");
    manager.mailbox().drain();
    let agent_id = manager.registry().get("/root/worker").unwrap().agent_id;

    let req = SendInputRequest {
        target: agent_id,
        message: Some("more context".to_string()),
        items: None,
        interrupt: None,
    };
    let out = run_handler(&send, &req).await.expect("send ok");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert!(body["submission_id"].is_string());

    // The communication landed on the manager's mailbox.
    assert!(manager.mailbox().has_pending(), "input must be enqueued");
    assert!(sink.types().contains(&"subagent.input".to_string()));
}

#[tokio::test]
async fn send_input_interrupt_requires_store_backed_runtime() {
    let (manager, _sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let send = SendInputTool::new(deps);

    run_handler(&spawn, &spawn_args("worker", "work"))
        .await
        .expect("spawn ok");
    manager.mailbox().drain();
    manager
        .registry()
        .update_status("/root/worker", AgentStatus::Running);
    let agent_id = manager.registry().get("/root/worker").unwrap().agent_id;

    let err = run_handler(
        &send,
        &SendInputRequest {
            target: agent_id,
            message: Some("redirect now".to_string()),
            items: None,
            interrupt: Some(true),
        },
    )
    .await
    .expect_err("no-store interrupt must fail honestly");

    assert!(
        format!("{err:?}").contains("interrupt is only supported for store-backed agents"),
        "unexpected error: {err:?}"
    );

    let record = manager.registry().get("/root/worker").unwrap();
    assert_eq!(record.status, AgentStatus::Running);
    assert_eq!(record.last_task_message.as_deref(), Some("work"));
}

#[tokio::test]
async fn legacy_v1_send_and_close_reject_path_targets() {
    let (_manager, _sink, deps) = deps_with_fake();
    let send = SendInputTool::new(deps.clone());
    let close = CloseAgentTool::new_legacy(deps);

    let send_err = run_handler(
        &send,
        &SendInputRequest {
            target: "/root/worker".to_string(),
            message: Some("not by path".to_string()),
            items: None,
            interrupt: None,
        },
    )
    .await
    .expect_err("legacy send_input must reject path targets");
    assert!(format!("{send_err:?}").contains("target agent ids"));

    let close_err = run_handler(
        &close,
        &CloseAgentV1Request {
            target: "/root/worker".to_string(),
        },
    )
    .await
    .expect_err("legacy close_agent must reject path targets");
    assert!(format!("{close_err:?}").contains("target agent ids"));
}

#[tokio::test]
async fn legacy_v1_wait_rejects_invalid_agent_ids() {
    let (_manager, _sink, deps) = deps_with_fake();
    let wait = WaitAgentV1Tool::new(deps);

    let malformed = run_handler(
        &wait,
        &WaitAgentV1Request {
            targets: vec!["invalid".to_string()],
            timeout_ms: Some(10_000),
        },
    )
    .await
    .expect_err("legacy wait_agent must reject malformed thread ids");
    assert!(format!("{malformed:?}").contains("invalid agent id invalid"));

    let path = run_handler(
        &wait,
        &WaitAgentV1Request {
            targets: vec!["/root/worker".to_string()],
            timeout_ms: Some(10_000),
        },
    )
    .await
    .expect_err("legacy wait_agent must reject path targets");
    assert!(format!("{path:?}").contains("target agent ids"));
}

#[tokio::test]
async fn send_message_queues_without_trigger_and_followup_triggers() {
    let (manager, _sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let send = SendMessageTool::new(deps.clone());
    let followup = FollowupTaskTool::new(deps);

    run_handler(&spawn, &spawn_args("worker", "work"))
        .await
        .expect("spawn ok");
    manager.mailbox().drain();

    let out = run_handler(
        &send,
        &SendMessageRequest {
            target: "worker".to_string(),
            message: "queued note".to_string(),
        },
    )
    .await
    .expect("send_message ok");
    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.is_empty());
    let queued = manager.mailbox().drain();
    assert_eq!(queued.len(), 1);
    assert!(!queued[0].trigger_turn);

    run_handler(
        &followup,
        &FollowupTaskRequest {
            target: "worker".to_string(),
            message: "next task".to_string(),
        },
    )
    .await
    .expect("followup ok");
    let triggered = manager.mailbox().drain();
    assert_eq!(triggered.len(), 1);
    assert!(triggered[0].trigger_turn);
}

#[tokio::test]
async fn list_agents_reports_spawned_children() {
    let (_manager, _sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let list = ListAgentsTool::new(deps);

    run_handler(&spawn, &spawn_args("explore", "t"))
        .await
        .expect("spawn ok");

    let out = run_handler(&list, &ListAgentsRequest::default())
        .await
        .expect("list ok");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let agents = body["agents"].as_array().unwrap();
    assert_eq!(
        agents.len(),
        2,
        "root plus one spawned agent expected: {body}"
    );
    assert!(agents.iter().any(|agent| agent["agent_name"] == "/root"));
    assert!(agents.iter().any(|agent| agent["agent_name"]
        .as_str()
        .is_some_and(|p| p == "/root/explore")));
}

#[tokio::test]
async fn list_agents_filters_by_path_prefix() {
    let (_manager, _sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let list = ListAgentsTool::new(deps);

    run_handler(&spawn, &spawn_args("explore", "t"))
        .await
        .expect("spawn ok");

    let out = run_handler(
        &list,
        &ListAgentsRequest {
            path_prefix: Some("explore".to_string()),
        },
    )
    .await
    .expect("list ok");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let agents = body["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "only the prefix match expected: {body}");
    assert_eq!(agents[0]["agent_name"].as_str(), Some("/root/explore"));
}

#[tokio::test]
async fn v2_targets_use_strict_codex_paths() {
    let (manager, _sink, deps) = deps_with_fake();
    let send = SendMessageTool::new(deps.clone());
    let followup = FollowupTaskTool::new(deps);

    let err = run_handler(
        &send,
        &SendMessageRequest {
            target: "root".to_string(),
            message: "plain root is not v2 syntax".to_string(),
        },
    )
    .await
    .expect_err("bare root should be rejected by v2 path parsing");
    let err_text = format!("{err:?}");
    assert!(
        err_text.contains("agent_name `root` is reserved"),
        "unexpected error: {err_text}"
    );

    run_handler(
        &send,
        &SendMessageRequest {
            target: "/root".to_string(),
            message: "queue to root".to_string(),
        },
    )
    .await
    .expect("absolute /root is accepted for queue-only messages");
    let queued = manager.mailbox().drain();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].to_agent_path, "/root");
    assert!(!queued[0].trigger_turn);

    let err = run_handler(
        &followup,
        &FollowupTaskRequest {
            target: "/root".to_string(),
            message: "new root task".to_string(),
        },
    )
    .await
    .expect_err("followup_task must reject root targets");
    let err_text = format!("{err:?}");
    assert!(
        err_text.contains("Tasks can't be assigned to the root agent"),
        "unexpected error: {err_text}"
    );
}

#[tokio::test]
async fn list_agents_omits_closed_agents() {
    let (_manager, _sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let close = CloseAgentTool::new(deps.clone());
    let list = ListAgentsTool::new(deps);

    run_handler(&spawn, &spawn_args("worker", "work"))
        .await
        .expect("spawn ok");
    run_handler(
        &close,
        &CloseAgentRequest {
            target: "worker".to_string(),
        },
    )
    .await
    .expect("close ok");

    let out = run_handler(&list, &ListAgentsRequest::default())
        .await
        .expect("list ok");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let agents = body["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "only root should remain live: {body}");
    assert_eq!(agents[0]["agent_name"].as_str(), Some("/root"));
}

#[tokio::test]
async fn close_agent_marks_target_closed() {
    let (manager, sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let close = CloseAgentTool::new(deps);

    run_handler(&spawn, &spawn_args("worker", "work"))
        .await
        .expect("spawn ok");
    let out = run_handler(
        &close,
        &CloseAgentRequest {
            target: "worker".to_string(),
        },
    )
    .await
    .expect("close ok");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(
        body["previous_status"],
        serde_json::json!({ "completed": null })
    );
    assert_eq!(
        manager.registry().get("/root/worker").unwrap().status,
        AgentStatus::Shutdown
    );
    assert!(sink.types().contains(&"subagent.closed".to_string()));
}

#[tokio::test]
async fn followup_task_wakes_idle_store_backed_child_and_reports_completion() {
    let (_dir, store, root_id, child_id, _sink, mut deps) = deps_with_store_tree();
    {
        let store = store.lock().unwrap();
        store
            .set_status(&child_id, SessionStatus::Done)
            .expect("child idle");
    }
    seed_child_run_config_marker(&store, &child_id);

    let captured: Arc<Mutex<Vec<ChildAgentRunRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_runner = Arc::clone(&captured);
    let store_for_runner = store.clone();
    let runner = ChildAgentRunner::new(move |request| {
        if let Some(run_id) = request.run_id.as_deref() {
            store_for_runner
                .lock()
                .unwrap()
                .append_event(
                    &request.child_session_id,
                    "agent.run.started",
                    serde_json::json!({ "run_id": run_id }),
                )
                .unwrap();
        }
        captured_for_runner.lock().unwrap().push(request.clone());
        request
            .completion_handler
            .as_ref()
            .expect("followup wake should include a completion handler")
            .notify(ChildAgentRunCompletion::success(Some(
                "fresh result".to_string(),
            )))?;
        Ok(())
    });
    deps.child_runner = Some(runner);
    let followup = FollowupTaskTool::new(deps);

    let out = run_handler(
        &followup,
        &FollowupTaskRequest {
            target: "worker".to_string(),
            message: "next task".to_string(),
        },
    )
    .await
    .expect("followup ok");
    assert!(out.stdout.is_empty());

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].child_session_id, child_id);
    assert_eq!(requests[0].fork_turns.as_deref(), Some("none"));
    assert!(requests[0].run_id.is_some());
    assert_child_run_config_replayed(&requests[0]);
    drop(requests);

    let store = store.lock().unwrap();
    let child_mail = store.messages_for_agent(&child_id).unwrap();
    assert_eq!(child_mail.len(), 1);
    assert_eq!(child_mail[0].content, "next task");
    assert!(child_mail[0].trigger_turn);

    let parent_mail = store.messages_for_agent(&root_id).unwrap();
    assert_eq!(parent_mail.len(), 1);
    assert!(parent_mail[0].content.contains("<subagent_notification>"));
    assert!(!parent_mail[0].trigger_turn);
    assert!(store
        .events_for_session(&root_id)
        .unwrap()
        .iter()
        .any(|event| event.event_type == "agent.completed"));
}

#[tokio::test]
async fn store_completion_handler_ignores_stale_run_marker_after_restart() {
    let (_dir, store, root_id, child_id, _sink, _deps) = deps_with_store_tree();
    let old_run_id = "old-run".to_string();
    let new_run_id = "new-run".to_string();
    let old_handler = store_completion_handler(
        store.clone(),
        root_id.clone(),
        child_id.clone(),
        Some(old_run_id.clone()),
    );
    let new_handler = store_completion_handler(
        store.clone(),
        root_id.clone(),
        child_id.clone(),
        Some(new_run_id.clone()),
    );

    {
        let store = store.lock().unwrap();
        store
            .append_event(
                &child_id,
                "agent.run.started",
                serde_json::json!({ "run_id": old_run_id.as_str() }),
            )
            .unwrap();
        store
            .append_event(
                &child_id,
                "session.cancelled",
                serde_json::json!({ "reason": "interrupted by send_input" }),
            )
            .unwrap();
        store
            .append_event(
                &child_id,
                "agent.run.started",
                serde_json::json!({ "run_id": new_run_id.as_str() }),
            )
            .unwrap();
    }

    old_handler
        .notify(ChildAgentRunCompletion::success(Some(
            "stale result".to_string(),
        )))
        .expect("stale completion should be ignored without error");
    {
        let store = store.lock().unwrap();
        let parent_events = store.events_for_session(&root_id).unwrap();
        assert!(
            parent_events.iter().all(|event| {
                event.event_type != "agent.completed" && event.event_type != "agent.failed"
            }),
            "stale run must not notify parent: {parent_events:?}"
        );
        assert!(store.messages_for_agent(&root_id).unwrap().is_empty());
    }

    new_handler
        .notify(ChildAgentRunCompletion::success(Some(
            "fresh result".to_string(),
        )))
        .expect("current completion should notify");
    let store = store.lock().unwrap();
    let parent_events = store.events_for_session(&root_id).unwrap();
    assert_eq!(
        parent_events
            .iter()
            .filter(|event| event.event_type == "agent.completed")
            .count(),
        1
    );
    let parent_mail = store.messages_for_agent(&root_id).unwrap();
    assert_eq!(parent_mail.len(), 1);
    assert!(parent_mail[0].content.contains("fresh result"));
}

#[tokio::test]
async fn store_completion_handler_ignores_completion_after_close() {
    let (_dir, store, root_id, child_id, _sink, _deps) = deps_with_store_tree();
    let run_id = "closed-run".to_string();
    let handler = store_completion_handler(
        store.clone(),
        root_id.clone(),
        child_id.clone(),
        Some(run_id.clone()),
    );

    {
        let store = store.lock().unwrap();
        store
            .append_event(
                &child_id,
                "agent.run.started",
                serde_json::json!({ "run_id": run_id.as_str() }),
            )
            .unwrap();
        store
            .close_child_agent(&child_id, "closed by close_agent")
            .unwrap();
    }

    handler
        .notify(ChildAgentRunCompletion::success(Some(
            "late result".to_string(),
        )))
        .expect("late completion after close should be ignored without error");

    let store = store.lock().unwrap();
    let child_summary = store.agent_summary_for_child(&child_id).unwrap().unwrap();
    assert_eq!(child_summary.status, "closed");
    let parent_events = store.events_for_session(&root_id).unwrap();
    assert!(
        parent_events.iter().all(|event| {
            event.event_type != "agent.completed" && event.event_type != "agent.failed"
        }),
        "closed child must not notify parent: {parent_events:?}"
    );
    assert!(store.messages_for_agent(&root_id).unwrap().is_empty());
}

#[tokio::test]
async fn store_completion_handler_writes_current_run_completion_once() {
    let (_dir, store, root_id, child_id, _sink, _deps) = deps_with_store_tree();
    let run_id = "once-run".to_string();
    let handler = store_completion_handler(
        store.clone(),
        root_id.clone(),
        child_id.clone(),
        Some(run_id.clone()),
    );
    let handler_clone = handler.clone();

    {
        let store = store.lock().unwrap();
        store
            .append_event(
                &child_id,
                "agent.run.started",
                serde_json::json!({ "run_id": run_id.as_str() }),
            )
            .unwrap();
    }

    handler
        .notify(ChildAgentRunCompletion::success(Some(
            "first result".to_string(),
        )))
        .expect("first completion should notify");
    handler_clone
        .notify(ChildAgentRunCompletion::failure("late failure"))
        .expect("duplicate completion should be ignored without error");

    let store = store.lock().unwrap();
    let parent_events = store.events_for_session(&root_id).unwrap();
    assert_eq!(
        parent_events
            .iter()
            .filter(|event| event.event_type == "agent.completed")
            .count(),
        1
    );
    assert!(
        parent_events
            .iter()
            .all(|event| event.event_type != "agent.failed"),
        "duplicate failure must not overwrite first completion: {parent_events:?}"
    );
    let parent_mail = store.messages_for_agent(&root_id).unwrap();
    assert_eq!(parent_mail.len(), 1);
    assert!(parent_mail[0].content.contains("first result"));
}

#[tokio::test]
async fn send_input_interrupt_cancels_and_restarts_store_backed_child() {
    let (_dir, store, _root_id, child_id, _sink, mut deps) = deps_with_store_tree();
    seed_child_run_config_marker(&store, &child_id);
    let captured: Arc<Mutex<Vec<ChildAgentRunRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_runner = Arc::clone(&captured);
    deps.child_runner = Some(ChildAgentRunner::new(move |request| {
        captured_for_runner.lock().unwrap().push(request);
        Ok(())
    }));
    let send = SendInputTool::new(deps);

    let out = run_handler(
        &send,
        &SendInputRequest {
            target: child_id.clone(),
            message: Some("redirect now".to_string()),
            items: None,
            interrupt: Some(true),
        },
    )
    .await
    .expect("interrupting send_input should succeed");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert!(body["submission_id"].is_string());

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].child_session_id, child_id);
    assert_eq!(requests[0].fork_turns.as_deref(), Some("none"));
    assert_eq!(requests[0].message, "redirect now");
    assert_child_run_config_replayed(&requests[0]);
    drop(requests);

    let store = store.lock().unwrap();
    let child = store.load_session(&child_id).unwrap().unwrap();
    assert_eq!(child.status, SessionStatus::Created);
    let child_events = store.events_for_session(&child_id).unwrap();
    assert!(child_events
        .iter()
        .any(|event| event.event_type == "session.cancel_requested"));
    let child_mail = store.messages_for_agent(&child_id).unwrap();
    assert_eq!(child_mail.len(), 1);
    assert!(child_mail[0].trigger_turn);
}

#[tokio::test]
async fn send_input_items_preserves_structured_user_input_for_store_child() {
    let (_dir, store, _root_id, child_id, _sink, deps) = deps_with_store_tree();
    let send = SendInputTool::new(deps);

    run_handler(
        &send,
        &SendInputRequest {
            target: child_id.clone(),
            message: None,
            items: Some(vec![LegacyInputItem {
                r#type: Some("text".to_string()),
                text: Some("preserve me".to_string()),
                image_url: None,
                path: None,
                name: None,
                detail: None,
                text_elements: None,
            }]),
            interrupt: Some(false),
        },
    )
    .await
    .expect("structured send_input should succeed");

    let store = store.lock().unwrap();
    let child_mail = store.messages_for_agent(&child_id).unwrap();
    assert_eq!(child_mail.len(), 1);
    assert_eq!(child_mail[0].content, "preserve me");
    assert_eq!(child_mail[0].input_kind, "user_input");
    assert_eq!(
        child_mail[0].input_items,
        Some(serde_json::json!([{ "type": "text", "text": "preserve me" }]))
    );
}

#[tokio::test]
async fn send_input_items_preserves_structured_user_input_for_manager_fallback() {
    let (manager, _sink, deps) = deps_with_fake();
    let spawn = SpawnAgentV1Tool::new(deps.clone());
    let spawned = run_handler(
        &spawn,
        &SpawnAgentV1Request {
            message: Some("start child".to_string()),
            items: None,
            agent_type: None,
            fork_context: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
        },
    )
    .await
    .expect("spawn child");
    let body: serde_json::Value = serde_json::from_str(&spawned.stdout).unwrap();
    let agent_id = body["agent_id"].as_str().unwrap().to_string();
    manager.mailbox().drain();

    let send = SendInputTool::new(deps);
    run_handler(
        &send,
        &SendInputRequest {
            target: agent_id,
            message: None,
            items: Some(vec![LegacyInputItem {
                r#type: Some("text".to_string()),
                text: Some("preserve me".to_string()),
                image_url: None,
                path: None,
                name: None,
                detail: None,
                text_elements: None,
            }]),
            interrupt: Some(false),
        },
    )
    .await
    .expect("structured send_input should succeed");

    let drained = manager.mailbox().drain();
    let sent = drained
        .iter()
        .find(|message| message.prompt == "preserve me")
        .expect("structured fallback message");
    assert!(sent.items.is_empty());
    assert_eq!(
        sent.input_items,
        Some(serde_json::json!([{ "type": "text", "text": "preserve me" }]))
    );
}

#[tokio::test]
async fn wait_closed_completed_store_child_reports_shutdown() {
    let (_dir, store, _root_id, child_id, _sink, _deps) = deps_with_store_tree();
    let store = store.lock().unwrap();
    store.set_status(&child_id, SessionStatus::Done).unwrap();
    store
        .append_event(
            &child_id,
            "session.done",
            serde_json::json!({ "result": "finished before close" }),
        )
        .unwrap();
    store.set_child_agent_status(&child_id, "closed").unwrap();

    let statuses = final_statuses_for_v1_wait(&store, &[child_id.as_str()]).unwrap();
    assert_eq!(statuses["/root/worker"], serde_json::json!("shutdown"));
}

#[tokio::test]
async fn send_input_wakes_resumed_store_backed_child() {
    let (_dir, store, _root_id, child_id, _sink, mut deps) = deps_with_store_tree();
    {
        let store = store.lock().unwrap();
        store.close_child_agent(&child_id, "test close").unwrap();
    }

    let resume = ResumeAgentTool::new(deps.clone());
    let resumed = run_handler(
        &resume,
        &ResumeAgentRequest {
            id: child_id.clone(),
        },
    )
    .await
    .expect("resume should reopen child");
    let body: serde_json::Value = serde_json::from_str(&resumed.stdout).unwrap();
    assert_eq!(body["status"], "pending_init");

    let captured: Arc<Mutex<Vec<ChildAgentRunRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_runner = Arc::clone(&captured);
    deps.child_runner = Some(ChildAgentRunner::new(move |request| {
        captured_for_runner.lock().unwrap().push(request);
        Ok(())
    }));
    let send = SendInputTool::new(deps);

    run_handler(
        &send,
        &SendInputRequest {
            target: child_id.clone(),
            message: Some("continue after resume".to_string()),
            items: None,
            interrupt: None,
        },
    )
    .await
    .expect("send_input after resume should wake the child runner");

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].child_session_id, child_id);
    assert_eq!(requests[0].message, "continue after resume");
}

#[tokio::test]
async fn resume_completed_store_backed_child_restarts_target() {
    let (_dir, store, _root_id, child_id, _sink, mut deps) = deps_with_store_tree();
    seed_child_run_config_marker(&store, &child_id);
    {
        let store = store.lock().unwrap();
        store.set_status(&child_id, SessionStatus::Done).unwrap();
        store.set_child_agent_status(&child_id, "done").unwrap();
    }
    let captured: Arc<Mutex<Vec<ChildAgentRunRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_runner = Arc::clone(&captured);
    deps.child_runner = Some(ChildAgentRunner::new(move |request| {
        captured_for_runner.lock().unwrap().push(request);
        Ok(())
    }));
    let resume = ResumeAgentTool::new(deps);

    let resumed = run_handler(
        &resume,
        &ResumeAgentRequest {
            id: child_id.clone(),
        },
    )
    .await
    .expect("resume should restart completed child");
    let body: serde_json::Value = serde_json::from_str(&resumed.stdout).unwrap();
    assert_eq!(body["status"], "pending_init");
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].child_session_id, child_id);
    assert_child_run_config_replayed(&requests[0]);
}

#[tokio::test]
async fn store_backed_tools_use_durable_agent_tree() {
    let (_dir, store, root_id, child_id, sink, mut deps) = deps_with_store_tree();
    let grandchild_id = {
        let store = store.lock().unwrap();
        store
            .create_child_session(
                &child_id,
                std::path::Path::new("/tmp"),
                Some("/root/worker/helper"),
                Some("Helper"),
                Some("explorer"),
            )
            .unwrap()
            .id
    };
    let cleaned_sessions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let cleaned_sessions_for_callback = Arc::clone(&cleaned_sessions);
    deps.cleanup_session_runtime = Some(Arc::new(move |session_id| {
        cleaned_sessions_for_callback
            .lock()
            .unwrap()
            .push(session_id.to_string());
        1
    }));
    let send = SendMessageTool::new(deps.clone());
    let list = ListAgentsTool::new(deps.clone());
    let close = CloseAgentTool::new(deps);

    let sent = run_handler(
        &send,
        &SendMessageRequest {
            target: "worker".to_string(),
            message: "durable note".to_string(),
        },
    )
    .await
    .expect("store send_message ok");
    assert!(sent.stdout.is_empty());

    {
        let store = store.lock().unwrap();
        let mail = store.messages_for_agent(&child_id).unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].content, "durable note");
        assert!(!mail[0].trigger_turn);
        assert!(store
            .events_for_session(&root_id)
            .unwrap()
            .iter()
            .any(|event| event.event_type == "agent.message"));
    }

    let listed = run_handler(&list, &ListAgentsRequest::default())
        .await
        .expect("store list ok");
    let list_body: serde_json::Value = serde_json::from_str(&listed.stdout).unwrap();
    let agents = list_body["agents"].as_array().unwrap();
    let worker = agents
        .iter()
        .find(|agent| agent["agent_name"] == "/root/worker")
        .expect("worker listed");
    assert_eq!(worker["last_task_message"].as_str(), Some("durable note"));

    let closed = run_handler(
        &close,
        &CloseAgentRequest {
            target: "worker".to_string(),
        },
    )
    .await
    .expect("store close ok");
    let closed_body: serde_json::Value = serde_json::from_str(&closed.stdout).unwrap();
    assert!(closed_body.get("agent_id").is_none());
    {
        let store = store.lock().unwrap();
        assert_eq!(
            store
                .agent_summary_for_child(&child_id)
                .unwrap()
                .unwrap()
                .status,
            "closed"
        );
    }
    let cleaned_sessions = cleaned_sessions.lock().unwrap();
    assert_eq!(
        cleaned_sessions.as_slice(),
        &[child_id.clone(), grandchild_id]
    );
    assert!(sink.types().contains(&"subagent.input".to_string()));
    assert!(sink.types().contains(&"subagent.closed".to_string()));
}

#[tokio::test]
async fn spawn_failure_surfaces_as_tool_error() {
    let manager = Arc::new(SubagentManager::with_config(
        Arc::new(FailingSpawner),
        RoleRegistry::new(),
        DEFAULT_AGENT_MAX_DEPTH,
    ));
    let deps = SubagentToolDeps {
        manager,
        parent: ParentContext {
            agent_path: "/root".to_string(),
            depth: 0,
            base_config: AgentConfigLayer::base("m", "p"),
        },
        sink: Arc::new(CaptureSink::default()),
        session_id: "s".to_string(),
        store: None,
        child_runner: None,
        cleanup_session_runtime: None,
        spawn_gate: Arc::new(tokio::sync::Mutex::new(())),
        wait_timeouts: Default::default(),
        hide_spawn_agent_metadata: false,
        max_concurrent_threads_per_session: None,
    };
    let spawn = SpawnAgentTool::new(deps);
    let err = run_handler(&spawn, &spawn_args("explore", "x"))
        .await
        .expect_err("spawn must error when the spawner fails");
    match err {
        ToolError::Other(e) => assert!(
            e.to_string().contains("spawn_agent failed"),
            "error should name the failure: {e}"
        ),
        other => panic!("expected ToolError::Other, got {other:?}"),
    }
}

#[tokio::test]
async fn legacy_spawn_rejects_depth_limit_like_codex_v1() {
    let (_manager, _sink, mut deps) = deps_with_fake();
    deps.parent.depth = DEFAULT_AGENT_MAX_DEPTH;
    let spawn = SpawnAgentV1Tool::new(deps);

    let err = run_handler(
        &spawn,
        &SpawnAgentV1Request {
            message: Some("too deep".to_string()),
            items: None,
            agent_type: None,
            fork_context: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
        },
    )
    .await
    .expect_err("legacy v1 spawn should reject past depth limit");

    assert!(format!("{err:?}").contains("Agent depth limit reached. Solve the task yourself."));
}
