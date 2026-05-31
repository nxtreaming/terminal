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

use serde_json::json;

use super::*;
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
            registry.update_status(&spec.agent_path, AgentStatus::Completed);
            mailbox.enqueue(InterAgentCommunication::new(
                spec.agent_path.clone(),
                "/root",
                Vec::new(),
                "child completed",
                true,
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
    };
    (manager, sink, deps)
}

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "c".to_string(),
        tool_name: "subagent".to_string(),
        cwd: std::env::temp_dir(),
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
        agent_type: None,
        model: None,
        reasoning_effort: None,
        service_tier: None,
        fork_turns: None,
    }
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
    assert!(
        body.get("agent_path")
            .and_then(|v| v.as_str())
            .is_some_and(|p| p.starts_with("/root/explore_")),
        "spawn must return the child agent_path: {body}"
    );
    assert!(body.get("agent_id").and_then(|v| v.as_str()).is_some());

    // The spawn registered the child in the manager's registry.
    assert_eq!(manager.list_agents().len(), 1);
    // A durable spawned event was emitted.
    assert!(
        sink.types().contains(&"subagent.spawned".to_string()),
        "expected a subagent.spawned event, got {:?}",
        sink.types()
    );
}

#[tokio::test]
async fn wait_returns_child_completion_via_mailbox() {
    let (_manager, sink, deps) = deps_with_fake();
    let spawn = SpawnAgentTool::new(deps.clone());
    let wait = WaitAgentTool::new(deps);

    let out = run_handler(&spawn, &spawn_args("worker", "work"))
        .await
        .expect("spawn ok");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let agent_path = body["agent_path"].as_str().unwrap().to_string();

    let req = WaitAgentRequest {
        agent_path: agent_path.clone(),
        timeout_secs: Some(2),
    };
    let wait_out = run_handler(&wait, &req).await.expect("wait ok");
    let wbody: serde_json::Value = serde_json::from_str(&wait_out.stdout).unwrap();
    assert_eq!(wbody["status"].as_str(), Some("completed"));
    assert_eq!(wbody["timed_out"].as_bool(), Some(false));

    assert!(
        sink.types().contains(&"subagent.completed".to_string()),
        "expected a subagent.completed event, got {:?}",
        sink.types()
    );
}

#[tokio::test]
async fn send_input_enqueues_and_emits_event() {
    let (manager, sink, deps) = deps_with_fake();
    let send = SendInputTool::new(deps);

    let req = SendInputRequest {
        agent_path: "/root/worker_1".to_string(),
        message: "more context".to_string(),
    };
    let out = run_handler(&send, &req).await.expect("send ok");
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["delivered"].as_bool(), Some(true));

    // The communication landed on the manager's mailbox.
    assert!(manager.mailbox().has_pending(), "input must be enqueued");
    assert!(sink.types().contains(&"subagent.input".to_string()));
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
    assert_eq!(agents.len(), 1, "one spawned agent expected: {body}");
    assert!(agents[0]["agent_path"]
        .as_str()
        .is_some_and(|p| p.starts_with("/root/explore_")));
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
