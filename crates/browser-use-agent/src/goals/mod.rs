//! `goals/` — event-sourced GOAL state + TOKEN-BUDGET accounting + STEERING.
//!
//! Three pure submodules tied together by [`GoalManager`]:
//!   * [`state`]    — the event-sourced [`GoalState`] (a reducer over a
//!     [`GoalEvent`] log), mirroring the legacy event-folding goal lifecycle
//!     (`browser-use-core/src/goals.rs:28-87`).
//!   * [`budget`]   — token-budget accounting with the formula
//!     `max(input - cached, 0) + max(output, 0)`, in FULL PARITY with codex
//!     `core/src/goals.rs:1684-1688` (`non_cached_input() + output.max(0)`) and
//!     legacy `browser-use-core/src/goals.rs:330-334` (`input - cached_input +
//!     max(output, 0)`), reusing the shared byte->token heuristic from
//!     `context/accounting.rs`.
//!   * [`steering`] — the `goal_context` context-message renderer (envelope
//!     parity with legacy `lib.rs:9796-9805`) plus budget-threshold steering
//!     events emitted through the [`EventSink`] seam.
//!
//! [`GoalManager`] holds the goal event log (so the state is always replayable),
//! a [`BudgetState`], and an injected [`EventSink`]; it emits steering events on
//! goal-set and on budget warn/exhaust crossings.
//!
//! ## Integration seam (parity debt)
//!
//! This WP delivers the subsystem as a self-contained, unit-tested unit. It does
//! NOT yet wire into the async turn loop. In a later integration WP the turn
//! driver would, per response, call [`GoalManager::record_usage`] with the
//! response's `Usage`, then drain [`GoalManager::poll_steering`] (live crossings
//! are already emitted into the sink by `record_usage`/`set_goal`) and inject
//! [`GoalManager::goal_context_message`] into the next prompt. The legacy call
//! site is the per-turn accounting checkpoint
//! `append_goal_progress_accounting` (`browser-use-core/src/goals.rs:201-248`,
//! invoked from `lib.rs:2682/2907/3235`); that wiring is left as a seam.

pub mod budget;
pub mod state;
pub mod steering;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use browser_use_llm::schema::Usage;
use serde_json::Value;

use crate::events::EventSink;
use crate::events::PendingEvent;

pub use budget::BudgetLevel;
pub use budget::BudgetState;
pub use state::GoalEvent;
pub use state::GoalState;
pub use steering::GOAL_BUDGET_LIMIT_STEERING_EVENT;
pub use steering::GOAL_BUDGET_WARNING_EVENT;
pub use steering::GOAL_SET_EVENT;

/// Ties the event-sourced [`GoalState`] + [`BudgetState`] + steering together
/// behind an injected [`EventSink`].
///
/// The goal event log is the source of truth: [`GoalState`] is always
/// `state::replay(&self.events)`, and the budget's `tokens_used` is kept in
/// lock-step with the folded state's `tokens_used`. Steering events are emitted
/// synchronously through the sink as goal-set / threshold crossings occur.
pub struct GoalManager {
    session_id: String,
    events: Vec<GoalEvent>,
    state: GoalState,
    budget: BudgetState,
    sink: Arc<dyn EventSink>,
}

impl GoalManager {
    /// Create an empty manager (no goal yet) bound to `session_id` and `sink`.
    pub fn new(session_id: impl Into<String>, sink: Arc<dyn EventSink>) -> Self {
        Self {
            session_id: session_id.into(),
            events: Vec::new(),
            state: GoalState::default(),
            budget: BudgetState::new(None),
            sink,
        }
    }

    /// The current folded goal state.
    pub fn state(&self) -> &GoalState {
        &self.state
    }

    /// The current budget accounting state.
    pub fn budget(&self) -> &BudgetState {
        &self.budget
    }

    /// The raw goal event log (for persistence / replay / inspection).
    pub fn events(&self) -> &[GoalEvent] {
        &self.events
    }

    /// Whether a goal is currently active.
    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    /// Apply an event to the log, refold state, and resync the budget ceiling,
    /// returning the steering events produced by the transition.
    ///
    /// All public mutators funnel through here so the log, the folded state, and
    /// the budget can never drift, and so every transition gets exactly one
    /// steering diff. The returned events are ALSO emitted through the sink.
    fn apply(&mut self, event: GoalEvent) -> Vec<PendingEvent> {
        let prev_state = self.state.clone();
        let prev_budget = self.budget;

        self.events.push(event.clone());
        self.state = state::reduce(prev_state.clone(), &event);

        // Keep the budget ceiling in lock-step with the folded state, and keep
        // the accounted total equal to the folded `tokens_used` (the reducer is
        // the single source of truth for accumulation).
        self.budget.set_max(self.state.token_budget);
        self.resync_budget_total();

        let events = steering::steering_events(
            &self.session_id,
            &prev_state,
            &self.state,
            &prev_budget,
            &self.budget,
        );
        for ev in &events {
            self.sink.emit(ev.clone());
        }
        events
    }

    /// Force the budget's accounted total to match the folded `tokens_used`.
    fn resync_budget_total(&mut self) {
        let target = self.state.tokens_used;
        let current = self.budget.total_accounted();
        if target >= current {
            self.budget.account_tokens(target - current);
        } else {
            // The only way `tokens_used` drops is a clear/replace; rebuild.
            let mut fresh = BudgetState::new(self.state.token_budget);
            fresh.account_tokens(target);
            self.budget = fresh;
        }
    }

    /// Set (create) a goal. Mirrors codex/legacy `goal.created`.
    ///
    /// Emits a [`GOAL_SET_EVENT`] steering event (goal became active) through the
    /// sink and returns it.
    pub fn set_goal(
        &mut self,
        goal_id: impl Into<String>,
        text: impl Into<String>,
        token_budget: Option<i64>,
        turn_idx: Option<i64>,
    ) -> Vec<PendingEvent> {
        self.apply(GoalEvent::Set {
            goal_id: goal_id.into(),
            text: text.into(),
            status: None,
            token_budget,
            turn_idx,
        })
    }

    /// Update goal fields (status/text/budget). Mirrors `goal.updated`.
    pub fn update_goal(
        &mut self,
        status: Option<String>,
        text: Option<String>,
        token_budget: Option<i64>,
    ) -> Vec<PendingEvent> {
        self.apply(GoalEvent::Updated {
            status,
            text,
            token_budget,
        })
    }

    /// Mark the active goal complete.
    pub fn complete_goal(&mut self) -> Vec<PendingEvent> {
        self.apply(GoalEvent::Completed)
    }

    /// Clear / abandon the goal, resetting all goal + budget state.
    pub fn clear_goal(&mut self) -> Vec<PendingEvent> {
        self.apply(GoalEvent::Cleared)
    }

    /// Account a response's token usage against the active goal and emit any
    /// budget-threshold steering events that the increment crossed.
    ///
    /// The increment is `max(input - cached, 0) + max(output, 0)`
    /// (`budget::tokens_from_llm_usage`, full parity with codex
    /// `goals.rs:1684-1688` / legacy `goals.rs:330-334`). No-op (no event, no
    /// accounting) when no goal is active, matching legacy
    /// `append_goal_progress_accounting`'s active-goal guard
    /// (`browser-use-core/src/goals.rs:210-215`).
    pub fn record_usage(&mut self, usage: &Usage, time_used_seconds: i64) -> Vec<PendingEvent> {
        if !self.state.is_active() {
            return Vec::new();
        }
        let tokens = budget::tokens_from_llm_usage(usage);
        self.apply(GoalEvent::Accounted {
            tokens_used: tokens,
            time_used_seconds,
        })
    }

    /// Render the active goal as a `goal_context` context message, if any.
    ///
    /// Parity: legacy `goal_context_message`
    /// (`browser-use-core/src/goals.rs:115-145`).
    pub fn goal_context_message(&self) -> Option<Value> {
        steering::render_goal_context_message(&self.state)
    }

    /// Compute (without emitting) the steering events for the CURRENT budget
    /// level versus an `Ok` baseline. Useful for a fresh consumer that wants to
    /// observe the current crossing state on demand.
    ///
    /// Note: live crossings are emitted through the sink by the mutators; this
    /// is a pull-style convenience and does not re-emit.
    pub fn poll_steering(&self) -> Vec<PendingEvent> {
        let baseline = BudgetState::new(self.budget.max());
        steering::steering_events(
            &self.session_id,
            &GoalState::default(),
            &self.state,
            &baseline,
            &self.budget,
        )
    }
}
