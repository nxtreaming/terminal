//! `spawn_agent` / `wait_agent` / `send_input` / `list_agents` — the
//! model-callable subagent orchestration tools.
//!
//! These are THIN handlers over the existing [`SubagentManager`]
//! (`crate::subagents::manager`): the manager already owns the live-agent
//! registry, the EVENT-NOTIFY mailbox, depth enforcement, role application and
//! the [`ChildSpawner`](crate::subagents::ChildSpawner) seam. The handlers do
//! nothing but (a) parse the model's JSON args, (b) call into the manager, and
//! (c) emit durable lifecycle events so the TUI's subagent render updates.
//!
//! Parity:
//! - tool names + arg shapes: codex `multi_agents_spec.rs` (`spawn_agent` takes
//!   `task_name` + `message`; `wait` / `send_input` reference an agent). The
//!   `spawn_agent` request reuses [`SpawnAgentArgs`] verbatim (same
//!   `deny_unknown_fields` wire contract the subsystem already defines).
//! - lifecycle: a spawn enqueues a child through the manager and emits
//!   `subagent.spawned`; a wait drains the mailbox and emits `subagent.output` /
//!   `subagent.completed`; `send_input` enqueues an inter-agent communication.
//!
//! Each handler implements the [`ToolRuntime`] stack ONCE (like `done`): no
//! sandbox, no approval, never denied — they route through the orchestrator on
//! the SAME typed dispatch path as every other tool, returning the operation's
//! JSON result as the tool output `stdout`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::time::Duration;

use crate::events::{EventSink, PendingEvent};
use crate::subagents::mailbox::{AgentStatus, InterAgentCommunication};
use crate::subagents::manager::{ParentContext, SubagentManager};
use crate::subagents::spawn::SpawnAgentArgs;
use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::SandboxPreference;

/// Default `wait_agent` timeout when the model does not supply one. Generous so
/// a child that is genuinely running is not prematurely reported as "no change".
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 300;

/// Shared dependencies every subagent tool carries: the manager (lifecycle
/// owner) + the parent's context (for spawn) + a durable event sink (+ session
/// id) so lifecycle transitions are persisted for the TUI render.
///
/// Cloning is cheap (`Arc`s + a small `ParentContext`); each of the four tools
/// holds its own clone so they share one manager/registry/mailbox.
#[derive(Clone)]
pub struct SubagentToolDeps {
    /// The lifecycle owner. Spawn/wait/send/list all route through it.
    pub manager: Arc<SubagentManager>,
    /// The parent's canonical path/depth/base-config, threaded into every
    /// `spawn` so depth limits + sticky provider/tier are enforced.
    pub parent: ParentContext,
    /// Durable + UI event sink. `subagent.*` events are emitted here so the
    /// TUI's existing subagent render sees lifecycle transitions.
    pub sink: Arc<dyn EventSink>,
    /// The session the events are scoped to (the parent's session id).
    pub session_id: String,
}

impl SubagentToolDeps {
    fn emit(&self, event_type: &str, payload: serde_json::Value) {
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            event_type,
            payload,
        ));
    }
}

// ----------------------------------------------------------------------------
// spawn_agent
// ----------------------------------------------------------------------------

/// The `spawn_agent` tool: delegate a task to a freshly-spawned child agent.
///
/// The model's args are [`SpawnAgentArgs`] (`task_name` + `message`, with the
/// optional `agent_type` / `model` / `reasoning_effort` / `service_tier` /
/// `fork_turns` overrides). On success it returns the new child's
/// `{ agent_path, agent_id }` so the model can later `wait_agent` / `send_input`.
pub struct SpawnAgentTool {
    deps: SubagentToolDeps,
}

impl SpawnAgentTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<SpawnAgentArgs> for SpawnAgentTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &SpawnAgentArgs) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for SpawnAgentTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<SpawnAgentArgs, ExecOutput> for SpawnAgentTool {
    fn parallel_safe(&self, _req: &SpawnAgentArgs) -> bool {
        // Spawning just mints a handle + hands off to the spawner seam; the child
        // runs on its own task, so concurrent spawns do not contend.
        true
    }

    async fn run(
        &self,
        req: &SpawnAgentArgs,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        match self
            .deps
            .manager
            .spawn(req.clone(), &self.deps.parent)
            .await
        {
            Ok(handle) => {
                self.deps.emit(
                    "subagent.spawned",
                    json!({
                        "agent_path": handle.agent_path,
                        "agent_id": handle.agent_id,
                        "task_name": req.task_name,
                        "message": req.message,
                    }),
                );
                let body = json!({
                    "agent_path": handle.agent_path,
                    "agent_id": handle.agent_id,
                });
                Ok(ok_output(body))
            }
            // A spawn rejection (depth exceeded, bad task_name/fork_turns, spawner
            // failure) is surfaced to the model as a tool error naming the cause —
            // NOT a panic, matching codex's handler rejection.
            Err(err) => Err(ToolError::Other(anyhow::anyhow!(
                "spawn_agent failed: {err}"
            ))),
        }
    }
}

// ----------------------------------------------------------------------------
// wait_agent
// ----------------------------------------------------------------------------

/// Wire args for `wait_agent`: the agent to wait on + an optional timeout.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitAgentRequest {
    /// Canonical path of the child agent to wait on (from `spawn_agent`).
    pub agent_path: String,
    /// Optional wait budget in seconds (default [`DEFAULT_WAIT_TIMEOUT_SECS`]).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// The `wait_agent` tool: EVENT-NOTIFY wait until the child has news, then
/// report its status. Routes into [`SubagentManager::wait`] (no poll loop).
pub struct WaitAgentTool {
    deps: SubagentToolDeps,
}

impl WaitAgentTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<WaitAgentRequest> for WaitAgentTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &WaitAgentRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for WaitAgentTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<WaitAgentRequest, ExecOutput> for WaitAgentTool {
    fn parallel_safe(&self, _req: &WaitAgentRequest) -> bool {
        // A wait only blocks on the shared mailbox + reads the registry; it is
        // safe to overlap with other parallel-safe tools.
        true
    }

    async fn run(
        &self,
        req: &WaitAgentRequest,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let timeout = Duration::from_secs(req.timeout_secs.unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS));
        let status = self.deps.manager.wait(&req.agent_path, timeout).await;
        match status {
            Some(status) => {
                let event_type = match status {
                    AgentStatus::Completed => "subagent.completed",
                    AgentStatus::Failed => "subagent.failed",
                    _ => "subagent.output",
                };
                self.deps.emit(
                    event_type,
                    json!({
                        "agent_path": req.agent_path,
                        "status": status.as_str(),
                    }),
                );
                Ok(ok_output(json!({
                    "agent_path": req.agent_path,
                    "status": status.as_str(),
                    "timed_out": false,
                })))
            }
            // Timed out with no mailbox change: report it honestly (the model can
            // wait again). Not an error — a timeout is a valid observation.
            None => Ok(ok_output(json!({
                "agent_path": req.agent_path,
                "status": "running",
                "timed_out": true,
            }))),
        }
    }
}

// ----------------------------------------------------------------------------
// send_input
// ----------------------------------------------------------------------------

/// Wire args for `send_input`: deliver a message to a running child agent.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendInputRequest {
    /// Canonical path of the child agent to deliver input to.
    pub agent_path: String,
    /// The message/prompt body delivered to the child.
    pub message: String,
}

/// The `send_input` tool: enqueue an inter-agent communication onto the shared
/// mailbox (codex `enqueue_mailbox_communication`), waking the child.
pub struct SendInputTool {
    deps: SubagentToolDeps,
}

impl SendInputTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<SendInputRequest> for SendInputTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &SendInputRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for SendInputTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<SendInputRequest, ExecOutput> for SendInputTool {
    fn parallel_safe(&self, _req: &SendInputRequest) -> bool {
        true
    }

    async fn run(
        &self,
        req: &SendInputRequest,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // The communication is parent -> child; `trigger_turn = true` so the
        // delivery wakes the child into a fresh turn (codex `trigger_turn`).
        self.deps.manager.send_message(InterAgentCommunication::new(
            self.deps.parent.agent_path.clone(),
            req.agent_path.clone(),
            Vec::new(),
            req.message.clone(),
            true,
        ));
        self.deps.emit(
            "subagent.input",
            json!({
                "agent_path": req.agent_path,
                "message": req.message,
            }),
        );
        Ok(ok_output(json!({
            "delivered": true,
            "agent_path": req.agent_path,
        })))
    }
}

// ----------------------------------------------------------------------------
// list_agents
// ----------------------------------------------------------------------------

/// Wire args for `list_agents` (no arguments).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListAgentsRequest {}

/// The `list_agents` tool: a read-only snapshot of the live-agent registry.
pub struct ListAgentsTool {
    deps: SubagentToolDeps,
}

impl ListAgentsTool {
    pub fn new(deps: SubagentToolDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Approvable<ListAgentsRequest> for ListAgentsTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &ListAgentsRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for ListAgentsTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<ListAgentsRequest, ExecOutput> for ListAgentsTool {
    fn parallel_safe(&self, _req: &ListAgentsRequest) -> bool {
        true
    }

    async fn run(
        &self,
        _req: &ListAgentsRequest,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let agents: Vec<serde_json::Value> = self
            .deps
            .manager
            .list_agents()
            .into_iter()
            .map(|record| {
                json!({
                    "agent_path": record.agent_path,
                    "agent_id": record.agent_id,
                    "nickname": record.nickname,
                    "role": record.role,
                    "status": record.status.as_str(),
                    "depth": record.depth,
                })
            })
            .collect();
        Ok(ok_output(json!({ "agents": agents })))
    }
}

/// Render a JSON body as a successful tool output (exit 0, body on stdout).
fn ok_output(body: serde_json::Value) -> ExecOutput {
    ExecOutput {
        exit_code: 0,
        stdout: body.to_string(),
        stderr: String::new(),
    }
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod subagent_tests;
