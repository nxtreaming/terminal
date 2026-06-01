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
use browser_use_llm::schema::Usage;
use browser_use_protocol::EventRecord;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::events::{EventSink, PendingEvent};
use crate::goals::budget;
use crate::goals::state::status;
use crate::goals::steering;
use crate::goals::{GoalEvent, GoalManager};
use crate::session::SharedStore;
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
pub use crate::goals::{GOAL_ACCOUNTED_EVENT, GOAL_CLEARED_EVENT};

const MAX_THREAD_GOAL_OBJECTIVE_CHARS: usize = 4_000;
const COMPLETION_BUDGET_REPORT: &str = "Goal achieved. Report final usage from this tool result's structured goal fields. If `goal.tokenBudget` is present, include token usage from `goal.tokensUsed` and `goal.tokenBudget`. If `goal.timeUsedSeconds` is greater than 0, summarize elapsed time in a concise, human-friendly form appropriate to the response language.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GoalTimestamps {
    created_at: Option<i64>,
    updated_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ThreadGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadGoalSnapshot {
    thread_id: String,
    objective: String,
    status: ThreadGoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_budget: Option<i64>,
    tokens_used: i64,
    time_used_seconds: i64,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoalToolResponse {
    goal: Option<ThreadGoalSnapshot>,
    remaining_tokens: Option<i64>,
    completion_budget_report: Option<String>,
}

#[derive(Clone, Copy)]
enum CompletionBudgetReport {
    Include,
    Omit,
}

impl GoalToolResponse {
    fn new(goal: Option<ThreadGoalSnapshot>, report_mode: CompletionBudgetReport) -> Self {
        let remaining_tokens = goal.as_ref().and_then(|goal| {
            goal.token_budget
                .map(|budget| (budget - goal.tokens_used).max(0))
        });
        let completion_budget_report = match report_mode {
            CompletionBudgetReport::Include => goal
                .as_ref()
                .filter(|goal| goal.status == ThreadGoalStatus::Complete)
                .and_then(|goal| {
                    if goal.token_budget.is_none() && goal.time_used_seconds <= 0 {
                        None
                    } else {
                        Some(COMPLETION_BUDGET_REPORT.to_string())
                    }
                }),
            CompletionBudgetReport::Omit => None,
        };
        Self {
            goal,
            remaining_tokens,
            completion_budget_report,
        }
    }
}

fn protocol_status_from_str(value: Option<&str>) -> ThreadGoalStatus {
    match value.unwrap_or(status::ACTIVE) {
        status::PAUSED => ThreadGoalStatus::Paused,
        status::BLOCKED => ThreadGoalStatus::Blocked,
        status::USAGE_LIMITED | "usageLimited" => ThreadGoalStatus::UsageLimited,
        status::BUDGET_LIMITED | "budgetLimited" => ThreadGoalStatus::BudgetLimited,
        status::COMPLETE => ThreadGoalStatus::Complete,
        _ => ThreadGoalStatus::Active,
    }
}

fn local_status_from_protocol(status: ThreadGoalStatus) -> &'static str {
    match status {
        ThreadGoalStatus::Active => status::ACTIVE,
        ThreadGoalStatus::Paused => status::PAUSED,
        ThreadGoalStatus::Blocked => status::BLOCKED,
        ThreadGoalStatus::UsageLimited => status::USAGE_LIMITED,
        ThreadGoalStatus::BudgetLimited => status::BUDGET_LIMITED,
        ThreadGoalStatus::Complete => status::COMPLETE,
    }
}

fn unix_now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn validate_thread_goal_objective(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("goal objective must not be empty".to_string());
    }
    if value.chars().count() > MAX_THREAD_GOAL_OBJECTIVE_CHARS {
        return Err(format!(
            "goal objective must be at most {MAX_THREAD_GOAL_OBJECTIVE_CHARS} characters"
        ));
    }
    Ok(())
}

fn validate_goal_budget(value: Option<i64>) -> Result<(), String> {
    if value.is_some_and(|value| value <= 0) {
        return Err("goal budgets must be positive when provided".to_string());
    }
    Ok(())
}

struct NullGoalSink;

impl EventSink for NullGoalSink {
    fn emit(&self, _ev: PendingEvent) {}
}

pub fn goal_response_from_event_records(
    session_id: &str,
    records: &[EventRecord],
) -> serde_json::Value {
    let sink: Arc<dyn EventSink> = Arc::new(NullGoalSink);
    GoalStore::from_event_records(session_id.to_string(), sink, records)
        .response(CompletionBudgetReport::Omit)
}

/// A goal store shared by the goal tool family: a single [`GoalManager`] behind
/// a `Mutex` so all three tools (and a later turn-loop accountant) operate on
/// the same event-sourced state. The manager owns the durable [`EventSink`], so
/// `create_goal` (and budget-threshold crossings) emit through it automatically.
pub struct GoalStore {
    manager: Mutex<GoalManager>,
    source: Option<SharedStore>,
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
    timestamps: Mutex<GoalTimestamps>,
    budget_limit_context_reported_for: Mutex<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalAccountingResult {
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub status_changed_to_budget_limited: bool,
}

impl GoalStore {
    /// Build a store bound to `session_id`, emitting durable events through
    /// `sink`. The inner [`GoalManager`] shares the same sink.
    pub fn new(session_id: impl Into<String>, sink: Arc<dyn EventSink>) -> Self {
        let session_id = session_id.into();
        Self {
            manager: Mutex::new(GoalManager::new(session_id.clone(), sink.clone())),
            source: None,
            sink,
            session_id,
            counter: Mutex::new(0),
            timestamps: Mutex::new(GoalTimestamps::default()),
            budget_limit_context_reported_for: Mutex::new(None),
        }
    }

    /// Build a store by replaying previously persisted `goal.*` events for the
    /// session. Replayed events are folded into state without being emitted again.
    pub fn from_event_records(
        session_id: impl Into<String>,
        sink: Arc<dyn EventSink>,
        records: &[EventRecord],
    ) -> Self {
        let session_id = session_id.into();
        let replay = replay_goal_event_records(records);
        let counter = replay
            .events
            .iter()
            .filter(|event| matches!(event, GoalEvent::Set { .. }))
            .count() as u64;
        Self {
            manager: Mutex::new(GoalManager::from_events(
                session_id.clone(),
                sink.clone(),
                replay.events,
            )),
            source: None,
            sink,
            session_id,
            counter: Mutex::new(counter),
            timestamps: Mutex::new(replay.timestamps),
            budget_limit_context_reported_for: Mutex::new(None),
        }
    }

    /// Build a store that refreshes from the durable session event log before
    /// every public read or mutation. This keeps long-lived tool instances in
    /// sync with slash-command edits, cancellation pauses, and accounting
    /// emitted elsewhere in the same thread without adding mutable goal columns
    /// to the store schema.
    pub fn from_shared_store(
        session_id: impl Into<String>,
        sink: Arc<dyn EventSink>,
        source: SharedStore,
    ) -> Self {
        let session_id = session_id.into();
        let records = source
            .lock()
            .unwrap()
            .events_for_session(&session_id)
            .unwrap_or_default();
        let mut store = Self::from_event_records(session_id, sink, &records);
        store.source = Some(source);
        store
    }

    /// A fresh `goal-N` id from the monotonic counter.
    fn next_goal_id(&self) -> String {
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;
        format!("goal-{}", *counter)
    }

    fn refresh_from_source(&self) -> Result<(), String> {
        let Some(source) = self.source.as_ref() else {
            return Ok(());
        };
        let records = source
            .lock()
            .unwrap()
            .events_for_session(&self.session_id)
            .map_err(|error| error.to_string())?;
        self.refresh_from_event_records(&records);
        Ok(())
    }

    fn refresh_from_event_records(&self, records: &[EventRecord]) {
        let replay = replay_goal_event_records(records);
        let counter = replay
            .events
            .iter()
            .filter(|event| matches!(event, GoalEvent::Set { .. }))
            .count() as u64;
        let manager =
            GoalManager::from_events(self.session_id.clone(), self.sink.clone(), replay.events);
        let state = manager.state().clone();

        *self.manager.lock().unwrap() = manager;
        *self.counter.lock().unwrap() = counter;
        *self.timestamps.lock().unwrap() = replay.timestamps;

        let mut reported = self.budget_limit_context_reported_for.lock().unwrap();
        let reported_still_applies = state.status.as_deref() == Some(status::BUDGET_LIMITED)
            && reported.as_deref() == state.goal_id.as_deref();
        if !reported_still_applies {
            *reported = None;
        }
    }

    fn goal_snapshot(&self) -> Option<ThreadGoalSnapshot> {
        let mgr = self.manager.lock().unwrap();
        let state = mgr.state().clone();
        drop(mgr);

        let objective = state.text.clone()?;
        let timestamps = *self.timestamps.lock().unwrap();
        let now = unix_now_seconds();
        let created_at = timestamps.created_at.unwrap_or(now);
        let updated_at = timestamps.updated_at.unwrap_or(created_at);
        Some(ThreadGoalSnapshot {
            thread_id: self.session_id.clone(),
            objective,
            status: protocol_status_from_str(state.status.as_deref()),
            token_budget: state.token_budget,
            tokens_used: state.tokens_used,
            time_used_seconds: state.time_used_seconds,
            created_at,
            updated_at,
        })
    }

    /// The current folded goal state as a Codex-shaped JSON response.
    fn current_response(&self, report_mode: CompletionBudgetReport) -> serde_json::Value {
        serde_json::to_value(GoalToolResponse::new(self.goal_snapshot(), report_mode))
            .expect("goal response serializes")
    }

    /// The durable folded goal state as a Codex-shaped JSON response.
    fn response(&self, report_mode: CompletionBudgetReport) -> serde_json::Value {
        let _ = self.refresh_from_source();
        self.current_response(report_mode)
    }

    pub fn goal_context_text(&self) -> Option<String> {
        if self.refresh_from_source().is_err() {
            return None;
        }
        let state = self.manager.lock().unwrap().state().clone();
        if state.status.as_deref() == Some(status::BUDGET_LIMITED) {
            let goal_id = state.goal_id.clone().unwrap_or_default();
            let mut reported = self.budget_limit_context_reported_for.lock().unwrap();
            if reported.as_deref() == Some(goal_id.as_str()) {
                return None;
            }
            *reported = Some(goal_id);
        }
        steering::render_goal_context_text(&state)
    }

    pub fn account_usage(
        &self,
        usage: &Usage,
        time_used_seconds: i64,
    ) -> Option<GoalAccountingResult> {
        if self.refresh_from_source().is_err() {
            return None;
        }
        let tokens_used = budget::tokens_from_llm_usage(usage);
        let time_used_seconds = time_used_seconds.max(0);
        if tokens_used <= 0 && time_used_seconds == 0 {
            return None;
        }

        let (previous_status, current_status) = {
            let mut mgr = self.manager.lock().unwrap();
            if !mgr.state().is_active() {
                return None;
            }
            let previous_status = mgr.state().status.clone();
            let _ = mgr.record_usage(usage, time_used_seconds);
            let current_status = mgr.state().status.clone();
            (previous_status, current_status)
        };

        self.mark_updated_now();
        let snapshot = self.goal_snapshot();
        let status_changed_to_budget_limited = previous_status.as_deref()
            != Some(status::BUDGET_LIMITED)
            && current_status.as_deref() == Some(status::BUDGET_LIMITED);

        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            GOAL_ACCOUNTED_EVENT,
            json!({
                "type": GOAL_ACCOUNTED_EVENT,
                "tokens_used": tokens_used,
                "tokensUsed": tokens_used,
                "time_used_seconds": time_used_seconds,
                "timeUsedSeconds": time_used_seconds,
                "goal": snapshot.clone().map(serde_json::to_value).transpose().unwrap_or(None),
            }),
        ));

        if status_changed_to_budget_limited {
            self.sink.emit(PendingEvent::new(
                self.session_id.clone(),
                GOAL_UPDATED_EVENT,
                json!({
                    "type": GOAL_UPDATED_EVENT,
                    "status": status::BUDGET_LIMITED,
                    "goal": snapshot.map(serde_json::to_value).transpose().unwrap_or(None),
                }),
            ));
        }

        Some(GoalAccountingResult {
            tokens_used,
            time_used_seconds,
            status_changed_to_budget_limited,
        })
    }

    pub fn account_elapsed_seconds(&self, time_used_seconds: i64) -> Option<GoalAccountingResult> {
        self.account_usage(&Usage::default(), time_used_seconds)
    }

    pub fn account_elapsed_since_last_update(&self) -> Option<GoalAccountingResult> {
        if self.refresh_from_source().is_err() {
            return None;
        }
        let last_update = {
            let timestamps = *self.timestamps.lock().unwrap();
            timestamps.updated_at.or(timestamps.created_at)
        }?;
        let elapsed = unix_now_seconds().saturating_sub(last_update);
        if elapsed <= 0 {
            return None;
        }
        self.account_elapsed_seconds(elapsed)
    }

    pub fn create_goal_response(
        &self,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<serde_json::Value, String> {
        let objective = objective.trim().to_string();
        validate_thread_goal_objective(&objective)?;
        validate_goal_budget(token_budget)?;
        self.refresh_from_source()?;
        let goal_id = self.next_goal_id();
        {
            let mut mgr = self.manager.lock().unwrap();
            if mgr.state().text.is_some() {
                return Err(
                    "cannot create a new goal because this thread already has a goal; use update_goal only when the existing goal is complete"
                        .to_string(),
                );
            }
            let _ = mgr.set_goal(goal_id, objective, token_budget, None);
        }
        self.mark_created_now();
        self.clear_budget_limit_context_reported();
        Ok(self.current_response(CompletionBudgetReport::Omit))
    }

    pub fn replace_goal_response(
        &self,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<serde_json::Value, String> {
        let objective = objective.trim().to_string();
        validate_thread_goal_objective(&objective)?;
        validate_goal_budget(token_budget)?;
        self.refresh_from_source()?;
        let goal_id = self.next_goal_id();
        let emitted_created = {
            let mut mgr = self.manager.lock().unwrap();
            let emitted = mgr.set_goal(goal_id, objective, token_budget, None);
            emitted
                .iter()
                .any(|event| event.event_type == crate::goals::GOAL_SET_EVENT)
        };
        self.mark_created_now();
        self.clear_budget_limit_context_reported();
        if !emitted_created {
            self.emit_replacement_created_event();
        }
        Ok(self.current_response(CompletionBudgetReport::Omit))
    }

    fn emit_replacement_created_event(&self) {
        let Some(goal) = self.goal_snapshot() else {
            return;
        };
        let goal_value = serde_json::to_value(&goal).expect("goal snapshot serializes");
        let objective = goal.objective.clone();
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            crate::goals::GOAL_SET_EVENT,
            json!({
                "type": crate::goals::GOAL_SET_EVENT,
                "text": objective.clone(),
                "objective": objective,
                "status": local_status_from_protocol(goal.status),
                "token_budget": goal.token_budget,
                "tokenBudget": goal.token_budget,
                "createdAt": goal.created_at,
                "updatedAt": goal.updated_at,
                "goal": goal_value,
            }),
        ));
    }

    pub fn edit_goal_response(
        &self,
        objective: &str,
        next_status: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let objective = objective.trim().to_string();
        validate_thread_goal_objective(&objective)?;
        self.refresh_from_source()?;
        self.update_goal_fields_response(
            next_status,
            Some(objective.as_str()),
            None,
            CompletionBudgetReport::Omit,
        )
    }

    pub fn update_status_response(&self, next_status: &str) -> Result<serde_json::Value, String> {
        let report_mode = if next_status == status::COMPLETE {
            CompletionBudgetReport::Include
        } else {
            CompletionBudgetReport::Omit
        };
        self.update_goal_fields_response(Some(next_status), None, None, report_mode)
    }

    pub fn clear_goal_response(&self) -> Result<serde_json::Value, String> {
        self.refresh_from_source()?;
        {
            let mut mgr = self.manager.lock().unwrap();
            if mgr.state().text.is_none() {
                return Err("no goal set for this task".to_string());
            }
            let _ = mgr.clear_goal();
        }
        *self.timestamps.lock().unwrap() = GoalTimestamps::default();
        self.clear_budget_limit_context_reported();
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            GOAL_CLEARED_EVENT,
            json!({ "type": GOAL_CLEARED_EVENT }),
        ));
        Ok(self.current_response(CompletionBudgetReport::Omit))
    }

    fn update_goal_fields_response(
        &self,
        status: Option<&str>,
        text: Option<&str>,
        token_budget: Option<i64>,
        report_mode: CompletionBudgetReport,
    ) -> Result<serde_json::Value, String> {
        validate_goal_budget(token_budget)?;
        self.refresh_from_source()?;
        {
            let mut mgr = self.manager.lock().unwrap();
            if mgr.state().text.is_none() {
                return Err("no active goal to update".to_string());
            }
            let _ = mgr.update_goal(
                status.map(str::to_string),
                text.map(str::to_string),
                token_budget,
            );
        }
        self.mark_updated_now();
        if status != Some(status::BUDGET_LIMITED) {
            self.clear_budget_limit_context_reported();
        }
        let snapshot = self.current_response(report_mode);
        let mut payload = json!({
            "type": GOAL_UPDATED_EVENT,
            "goal": snapshot.get("goal").cloned().unwrap_or(serde_json::Value::Null),
        });
        if let serde_json::Value::Object(obj) = &mut payload {
            if let Some(status) = status {
                obj.insert("status".to_string(), json!(status));
            }
            if let Some(text) = text {
                obj.insert("text".to_string(), json!(text));
                obj.insert("objective".to_string(), json!(text));
            }
            if let Some(token_budget) = token_budget {
                obj.insert("token_budget".to_string(), json!(token_budget));
                obj.insert("tokenBudget".to_string(), json!(token_budget));
            }
        }
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            GOAL_UPDATED_EVENT,
            payload,
        ));
        Ok(snapshot)
    }

    fn clear_budget_limit_context_reported(&self) {
        *self.budget_limit_context_reported_for.lock().unwrap() = None;
    }

    fn mark_created_now(&self) {
        let now = unix_now_seconds();
        let mut timestamps = self.timestamps.lock().unwrap();
        timestamps.created_at.get_or_insert(now);
        timestamps.updated_at = Some(now);
    }

    fn mark_updated_now(&self) {
        self.timestamps.lock().unwrap().updated_at = Some(unix_now_seconds());
    }
}

struct GoalReplay {
    events: Vec<GoalEvent>,
    timestamps: GoalTimestamps,
}

fn replay_goal_event_records(records: &[EventRecord]) -> GoalReplay {
    let mut events = Vec::new();
    let mut timestamps = GoalTimestamps::default();

    for record in records {
        let Some(event) = goal_event_from_record(record) else {
            continue;
        };
        let root = &record.payload;
        let goal = root.get("goal").unwrap_or(root);
        let record_seconds = record.ts_ms.div_euclid(1000);
        let created_at =
            timestamp_field(root, goal, &["createdAt", "created_at"]).unwrap_or(record_seconds);
        let updated_at =
            timestamp_field(root, goal, &["updatedAt", "updated_at"]).unwrap_or(record_seconds);

        match &event {
            GoalEvent::Set { .. } => {
                timestamps.created_at = Some(created_at);
                timestamps.updated_at = Some(updated_at);
            }
            GoalEvent::Updated { .. } | GoalEvent::Accounted { .. } | GoalEvent::Completed => {
                timestamps.updated_at = Some(updated_at);
            }
            GoalEvent::Cleared => {
                timestamps = GoalTimestamps::default();
            }
        }
        events.push(event);
    }

    GoalReplay { events, timestamps }
}

fn goal_event_from_record(record: &EventRecord) -> Option<GoalEvent> {
    let root = &record.payload;
    let goal = root.get("goal").unwrap_or(root);
    match record.event_type.as_str() {
        crate::goals::GOAL_SET_EVENT => {
            let text = string_field(root, goal, &["objective", "text"])?;
            Some(GoalEvent::Set {
                goal_id: string_field(root, goal, &["goal_id", "goalId"])
                    .unwrap_or_else(|| record.id.clone()),
                text,
                status: status_field(root, goal),
                token_budget: i64_field(root, goal, &["token_budget", "tokenBudget"]),
                turn_idx: i64_field(root, goal, &["turn_idx", "turnIdx"]),
            })
        }
        GOAL_UPDATED_EVENT => {
            let status = status_field(root, goal);
            let text = string_field(root, goal, &["objective", "text"]);
            let token_budget = i64_field(root, goal, &["token_budget", "tokenBudget"]);
            if status.is_none() && text.is_none() && token_budget.is_none() {
                return None;
            }
            Some(GoalEvent::Updated {
                status,
                text,
                token_budget,
            })
        }
        GOAL_ACCOUNTED_EVENT => {
            let tokens_used = i64_field(
                root,
                goal,
                &["tokens_used", "tokensUsed", "token_delta", "tokenDelta"],
            )
            .unwrap_or_default();
            let time_used_seconds = i64_field(
                root,
                goal,
                &[
                    "time_used_seconds",
                    "timeUsedSeconds",
                    "time_delta_seconds",
                    "timeDeltaSeconds",
                ],
            )
            .unwrap_or_default();
            if tokens_used == 0 && time_used_seconds == 0 {
                return None;
            }
            Some(GoalEvent::Accounted {
                tokens_used,
                time_used_seconds,
            })
        }
        GOAL_CLEARED_EVENT => Some(GoalEvent::Cleared),
        _ => None,
    }
}

fn string_field(
    root: &serde_json::Value,
    goal: &serde_json::Value,
    keys: &[&str],
) -> Option<String> {
    keys.iter()
        .find_map(|key| root.get(*key).and_then(serde_json::Value::as_str))
        .or_else(|| {
            keys.iter()
                .find_map(|key| goal.get(*key).and_then(serde_json::Value::as_str))
        })
        .map(str::to_string)
}

fn status_field(root: &serde_json::Value, goal: &serde_json::Value) -> Option<String> {
    string_field(root, goal, &["status"]).map(|value| {
        local_status_from_protocol(protocol_status_from_str(Some(value.as_str()))).to_string()
    })
}

fn i64_field(root: &serde_json::Value, goal: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| root.get(*key).and_then(serde_json::Value::as_i64))
        .or_else(|| {
            keys.iter()
                .find_map(|key| goal.get(*key).and_then(serde_json::Value::as_i64))
        })
}

fn timestamp_field(
    root: &serde_json::Value,
    goal: &serde_json::Value,
    keys: &[&str],
) -> Option<i64> {
    i64_field(root, goal, keys)
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
        Ok(ok_output(self.store.response(CompletionBudgetReport::Omit)))
    }
}

// ----------------------------------------------------------------------------
// create_goal
// ----------------------------------------------------------------------------

/// Wire args for `create_goal`.
///
/// `objective` is required, and `token_budget` is optional and must be positive
/// when provided. This mirrors Codex's goal tool contract.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateGoalRequest {
    pub objective: String,
    #[serde(default)]
    pub token_budget: Option<i64>,
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
        self.store
            .create_goal_response(&req.objective, req.token_budget)
            .map(ok_output)
            .map_err(|err| ToolError::Other(anyhow::anyhow!("create_goal: {err}")))
    }
}

// ----------------------------------------------------------------------------
// update_goal
// ----------------------------------------------------------------------------

/// Terminal statuses the model is allowed to set through `update_goal`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateGoalStatus {
    Complete,
    Blocked,
}

impl UpdateGoalStatus {
    fn as_local_str(self) -> &'static str {
        match self {
            UpdateGoalStatus::Complete => status::COMPLETE,
            UpdateGoalStatus::Blocked => status::BLOCKED,
        }
    }

    fn report_mode(self) -> CompletionBudgetReport {
        match self {
            UpdateGoalStatus::Complete => CompletionBudgetReport::Include,
            UpdateGoalStatus::Blocked => CompletionBudgetReport::Omit,
        }
    }
}

/// Wire args for `update_goal`. Codex allows the model to mark an existing goal
/// complete or blocked only; pause/resume/budget-limited are external controls.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateGoalRequest {
    pub status: UpdateGoalStatus,
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
        let _ = self.store.account_elapsed_since_last_update();
        self.store
            .update_goal_fields_response(
                Some(req.status.as_local_str()),
                None,
                None,
                req.status.report_mode(),
            )
            .map(ok_output)
            .map_err(|err| ToolError::Other(anyhow::anyhow!("update_goal: {err}")))
    }
}

#[cfg(test)]
#[path = "goal_tests.rs"]
mod goal_tests;
