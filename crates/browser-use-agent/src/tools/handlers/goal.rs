//! Goal tool family: `get_goal` / `create_goal` / `update_goal`.
//!
//! These are THIN handlers over the existing goals subsystem
//! ([`GoalManager`](crate::goals::GoalManager)): the manager already owns the
//! event-sourced [`GoalState`](crate::goals::GoalState), the budget accounting,
//! and the [`EventSink`] seam through which it emits `goal.created` (and budget
//! crossings). The handlers do nothing but (a) parse the model's JSON args,
//! (b) call into a shared [`GoalStore`], and (c) emit durable `goal.*` events so
//! the TUI render / resume-by-replay observe the lifecycle.
//!
//! Each handler implements the full [`ToolRuntime`] stack ONCE (like `done` /
//! the subagent tools): no sandbox, no approval, never denied — they route
//! through the orchestrator on the SAME typed dispatch path as every other tool,
//! returning the operation's JSON result as the tool output `stdout`.
//!
//! Parity:
//! - tool names + arg shapes mirror the codex goal-spec tool family
//!   (`goal_spec.rs` / `spec_plan.rs`): `get_goal`, `create_goal`,
//!   `update_goal`.
//! - event names reuse the goals module's existing constants:
//!   * `create_goal` -> [`GOAL_SET_EVENT`](crate::goals::GOAL_SET_EVENT)
//!     (`goal.created`), emitted by
//!     [`GoalManager::set_goal`](crate::goals::GoalManager::set_goal) through its
//!     sink.
//!   * `update_goal` -> [`GOAL_UPDATED_EVENT`] (`goal.updated`), emitted here
//!     (the manager's steering only fires on goal-set / budget crossings).
//!   Parity: legacy goal lifecycle events `goal.created` / `goal.updated`
//!   (`browser-use-core/src/constants.rs:126-127`).

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::events::{EventSink, PendingEvent};
use crate::goals::GoalManager;
use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::SandboxPreference;

/// Durable goal-updated event name.
///
/// Parity: legacy `GOAL_UPDATED_EVENT = "goal.updated"`
/// (`browser-use-core/src/constants.rs:127`). The created/budget events reuse the
/// goals module's own constants
/// ([`crate::goals::GOAL_SET_EVENT`] etc.), emitted by the manager.
pub const GOAL_UPDATED_EVENT: &str = "goal.updated";

/// A goal store shared by the goal tool family: a single [`GoalManager`] behind
/// a `Mutex` so all three tools (and a later turn-loop accountant) operate on
/// the same event-sourced state. The manager owns the durable [`EventSink`], so
/// `create_goal` (and budget-threshold crossings) emit through it automatically.
pub struct GoalStore {
    manager: Mutex<GoalManager>,
    /// A handle to the same sink the manager emits through, so `update_goal` can
    /// emit a `goal.updated` event (the manager's steering only fires on
    /// goal-set / budget crossings).
    sink: Arc<dyn EventSink>,
    /// The session id stamped on events emitted directly from a tool handler.
    session_id: String,
    /// Monotonic counter for deriving a stable goal id when the model omits one
    /// (dependency-free + deterministic, mirroring how other in-crate ids are
    /// minted).
    counter: Mutex<u64>,
}

impl GoalStore {
    /// Build a store bound to `session_id`, emitting durable events through
    /// `sink`. The inner [`GoalManager`] shares the same sink.
    pub fn new(session_id: impl Into<String>, sink: Arc<dyn EventSink>) -> Self {
        let session_id = session_id.into();
        Self {
            manager: Mutex::new(GoalManager::new(session_id.clone(), sink.clone())),
            sink,
            session_id,
            counter: Mutex::new(0),
        }
    }

    /// A fresh `goal-N` id from the monotonic counter.
    fn next_goal_id(&self) -> String {
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;
        format!("goal-{}", *counter)
    }

    /// The current folded goal state as a JSON snapshot.
    fn snapshot(&self) -> serde_json::Value {
        let mgr = self.manager.lock().unwrap();
        let state = mgr.state();
        json!({
            "active": state.is_active(),
            "goal_id": state.goal_id,
            "text": state.text,
            "status": state.status,
            "token_budget": state.token_budget,
            "tokens_used": state.tokens_used,
            "time_used_seconds": state.time_used_seconds,
            "created_turn_idx": state.created_turn_idx,
        })
    }
}

/// Render a JSON body as a successful tool output (exit 0, body on stdout).
/// Mirrors the subagent handlers' `ok_output`.
fn ok_output(body: serde_json::Value) -> ExecOutput {
    ExecOutput {
        exit_code: 0,
        stdout: body.to_string(),
        stderr: String::new(),
    }
}

// ----------------------------------------------------------------------------
// get_goal
// ----------------------------------------------------------------------------

/// Wire args for `get_goal` (no arguments).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetGoalRequest {}

/// The `get_goal` tool: report the current thread goal + token-budget usage.
pub struct GetGoalTool {
    store: Arc<GoalStore>,
}

impl GetGoalTool {
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Approvable<GetGoalRequest> for GetGoalTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &GetGoalRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for GetGoalTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<GetGoalRequest, ExecOutput> for GetGoalTool {
    fn parallel_safe(&self, _req: &GetGoalRequest) -> bool {
        true
    }

    async fn run(
        &self,
        _req: &GetGoalRequest,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        Ok(ok_output(self.store.snapshot()))
    }
}

// ----------------------------------------------------------------------------
// create_goal
// ----------------------------------------------------------------------------

/// Wire args for `create_goal`.
///
/// `goal_id` is optional; when omitted a stable id is derived from the store so
/// the model can `create_goal` with just an objective. `token_budget` is an
/// optional hard ceiling (legacy `ThreadGoalSnapshot::token_budget`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateGoalRequest {
    pub text: String,
    #[serde(default)]
    pub goal_id: Option<String>,
    #[serde(default)]
    pub token_budget: Option<i64>,
    #[serde(default)]
    pub turn_idx: Option<i64>,
}

/// The `create_goal` tool: set the active thread goal. Emits `goal.created`
/// through the shared [`GoalManager`] sink.
pub struct CreateGoalTool {
    store: Arc<GoalStore>,
}

impl CreateGoalTool {
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Approvable<CreateGoalRequest> for CreateGoalTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &CreateGoalRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for CreateGoalTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<CreateGoalRequest, ExecOutput> for CreateGoalTool {
    fn parallel_safe(&self, _req: &CreateGoalRequest) -> bool {
        // Mutates the shared goal state; keep it serial.
        false
    }

    async fn run(
        &self,
        req: &CreateGoalRequest,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        if req.text.trim().is_empty() {
            return Err(ToolError::Other(anyhow::anyhow!(
                "create_goal: goal text must not be empty"
            )));
        }
        let goal_id = req
            .goal_id
            .clone()
            .unwrap_or_else(|| self.store.next_goal_id());
        {
            let mut mgr = self.store.manager.lock().unwrap();
            // `set_goal` emits the durable `goal.created` (GOAL_SET_EVENT)
            // through the manager's sink.
            let _ = mgr.set_goal(goal_id, req.text.clone(), req.token_budget, req.turn_idx);
        }
        Ok(ok_output(self.store.snapshot()))
    }
}

// ----------------------------------------------------------------------------
// update_goal
// ----------------------------------------------------------------------------

/// Wire args for `update_goal`. Each present field overwrites; absent fields are
/// left unchanged (legacy `goal.updated` semantics, `goals.rs:70-82`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateGoalRequest {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub token_budget: Option<i64>,
}

/// The `update_goal` tool: update the active goal's status/text/budget. Folds the
/// update through the manager and emits a durable `goal.updated` event.
pub struct UpdateGoalTool {
    store: Arc<GoalStore>,
}

impl UpdateGoalTool {
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Approvable<UpdateGoalRequest> for UpdateGoalTool {
    type ApprovalKey = String;
    fn approval_keys(&self, _req: &UpdateGoalRequest) -> Vec<Self::ApprovalKey> {
        Vec::new()
    }
}

impl Sandboxable for UpdateGoalTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Never
    }
}

#[async_trait]
impl ToolRuntime<UpdateGoalRequest, ExecOutput> for UpdateGoalTool {
    fn parallel_safe(&self, _req: &UpdateGoalRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &UpdateGoalRequest,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        {
            let mut mgr = self.store.manager.lock().unwrap();
            if mgr.state().text.is_none() {
                return Err(ToolError::Other(anyhow::anyhow!(
                    "update_goal: no active goal to update"
                )));
            }
            // The manager's steering layer does not emit a generic `goal.updated`
            // (only goal-set + budget crossings), so fold the update and emit the
            // durable event from here.
            let _ = mgr.update_goal(req.status.clone(), req.text.clone(), req.token_budget);
        }
        let snapshot = self.store.snapshot();
        self.store.sink.emit(PendingEvent::new(
            self.store.session_id.clone(),
            GOAL_UPDATED_EVENT,
            json!({
                "type": GOAL_UPDATED_EVENT,
                "status": req.status,
                "text": req.text,
                "token_budget": req.token_budget,
                "goal": snapshot.clone(),
            }),
        ));
        Ok(ok_output(snapshot))
    }
}

#[cfg(test)]
#[path = "goal_tests.rs"]
mod goal_tests;
