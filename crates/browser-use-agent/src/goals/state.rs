//! Event-sourced goal state.
//!
//! The goal lifecycle is modelled as a log of [`GoalEvent`]s folded by a pure,
//! deterministic reducer ([`reduce`]) into a [`GoalState`]. This mirrors the
//! codex / legacy event-sourcing style:
//!   * codex `core/src/goals.rs` — goal lifecycle is rebuilt from a goal event
//!     log (`goal.created` / `goal.updated` / `goal.accounted`).
//!   * legacy `browser-use-core/src/goals.rs:49-104` — `fold_goal_event` +
//!     `goal_state_from_events` fold the same events into a `GoalState`; the
//!     event-name constants live at `constants.rs:126-129`
//!     (`GOAL_CREATED_EVENT = "goal.created"`,
//!     `GOAL_UPDATED_EVENT = "goal.updated"`,
//!     `GOAL_ACCOUNTING_EVENT = "goal.accounted"`).
//!
//! Unlike the legacy folder, which reads `serde_json::Value` payloads off a
//! persisted `EventRecord` log, this is a strongly-typed in-memory event enum so
//! the reducer is total and unit-testable without a `Store`. The folded fields
//! and their semantics (status set, `tokens_used` accumulates, budget set on
//! create/update) match legacy `GoalState` (`goals.rs:27-47`).

use serde::Deserialize;
use serde::Serialize;

/// Wire-stable status strings for a goal.
///
pub mod status {
    pub const ACTIVE: &str = "active";
    pub const PAUSED: &str = "paused";
    pub const COMPLETE: &str = "complete";
    pub const BLOCKED: &str = "blocked";
    pub const USAGE_LIMITED: &str = "usage_limited";
    pub const BUDGET_LIMITED: &str = "budget_limited";
}

/// One entry in the goal event log.
///
/// The variants map onto the codex/legacy goal lifecycle events:
///   * [`GoalEvent::Set`]      -> `goal.created`  (legacy `GOAL_CREATED_EVENT`)
///   * [`GoalEvent::Updated`]  -> `goal.updated`  (legacy `GOAL_UPDATED_EVENT`)
///   * [`GoalEvent::Accounted`]-> `goal.accounted`(legacy `GOAL_ACCOUNTING_EVENT`)
///
/// `Cleared` / `Completed` are convenience updates that resolve to a status
/// transition (legacy expresses these as `goal.updated` with a terminal
/// `status`); they are kept as distinct variants so the reducer and the steering
/// layer can reason about them directly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalEvent {
    /// A goal is set (created). Mirrors legacy `goal.created`
    /// (`browser-use-core/src/goals.rs:32-69`): sets id/objective/status/budget;
    /// `tokens_used`/`time_used_seconds` start at zero.
    Set {
        goal_id: String,
        text: String,
        /// Optional explicit status; defaults to `active` (legacy `goals.rs:51-55`).
        status: Option<String>,
        /// Optional token budget; `None` means unlimited (legacy `goals.rs:56`).
        token_budget: Option<i64>,
        /// Turn index the goal was created on.
        turn_idx: Option<i64>,
    },
    /// A goal field is updated. Mirrors legacy `goal.updated`
    /// (`browser-use-core/src/goals.rs:70-82`): each present field overwrites
    /// (legacy updates `status` / `updated_at_ms`).
    Updated {
        status: Option<String>,
        text: Option<String>,
        token_budget: Option<i64>,
    },
    /// A response's usage is accounted against the goal. Mirrors legacy
    /// `goal.accounted` (`browser-use-core/src/goals.rs:110-131`): the token
    /// delta accumulates (saturating), elapsed seconds accumulate.
    Accounted {
        tokens_used: i64,
        time_used_seconds: i64,
    },
    /// Mark the active goal complete. Resolves to a terminal `complete` status
    /// (legacy expresses this as `goal.updated` with `status = "complete"`).
    Completed,
    /// Clear / abandon the goal. Resets to the empty default state.
    Cleared,
}

/// Folded goal state rebuilt from the goal event log.
///
/// Field set + semantics mirror legacy `GoalState`
/// (`browser-use-core/src/goals.rs:27-47`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalState {
    pub goal_id: Option<String>,
    pub text: Option<String>,
    pub status: Option<String>,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub created_turn_idx: Option<i64>,
    pub time_used_seconds: i64,
}

impl GoalState {
    /// Whether a goal is currently active (created and still eligible for goal
    /// progress accounting).
    ///
    /// Parity: legacy continuation gate
    /// (`browser-use-core/src/goals.rs:213` `matches!(status, "active" |
    /// "budget_limited")`). `budget_limited` remains active enough for wrap-up
    /// accounting/steering; stopped statuses do not.
    pub fn is_active(&self) -> bool {
        self.text.is_some()
            && !matches!(
                self.status.as_deref(),
                Some(status::COMPLETE)
                    | Some(status::BLOCKED)
                    | Some(status::PAUSED)
                    | Some(status::USAGE_LIMITED)
            )
    }
}

fn apply_budget_limit_if_reached(state: &mut GoalState) {
    if !matches!(
        state.status.as_deref(),
        Some(status::ACTIVE) | Some(status::BUDGET_LIMITED)
    ) {
        return;
    }
    let Some(budget) = state.token_budget else {
        return;
    };
    if state.tokens_used >= budget {
        state.status = Some(status::BUDGET_LIMITED.to_string());
    }
}

/// Apply a single event to the state (the pure reducer step).
///
/// Total and deterministic: every `(state, event)` maps to exactly one next
/// state with no I/O. Mirrors the legacy per-event folding in
/// `latest_thread_goal_from_events` (`browser-use-core/src/goals.rs:28-87`).
pub fn reduce(mut state: GoalState, event: &GoalEvent) -> GoalState {
    match event {
        GoalEvent::Set {
            goal_id,
            text,
            status,
            token_budget,
            turn_idx,
        } => {
            state.goal_id = Some(goal_id.clone());
            state.text = Some(text.clone());
            state.status = Some(status.clone().unwrap_or_else(|| status::ACTIVE.to_string()));
            state.token_budget = *token_budget;
            state.created_turn_idx = *turn_idx;
            state.tokens_used = 0;
            state.time_used_seconds = 0;
            apply_budget_limit_if_reached(&mut state);
        }
        GoalEvent::Updated {
            status,
            text,
            token_budget,
        } => {
            if let Some(status) = status {
                state.status = Some(status.clone());
            }
            if let Some(text) = text {
                state.text = Some(text.clone());
            }
            if let Some(budget) = token_budget {
                state.token_budget = Some(*budget);
            }
            apply_budget_limit_if_reached(&mut state);
        }
        GoalEvent::Accounted {
            tokens_used,
            time_used_seconds,
        } => {
            if state.is_active() {
                state.tokens_used = state.tokens_used.saturating_add(*tokens_used);
                // Elapsed seconds ACCUMULATE across responses (parity: legacy
                // `goal.accounted` folds `time_used_seconds += delta`,
                // `browser-use-core/src/goals.rs:110-131`). This previously ASSIGNED
                // the per-response delta, dropping all prior elapsed time; accumulate
                // it (saturating) so the folded total is the sum over the log.
                state.time_used_seconds =
                    state.time_used_seconds.saturating_add(*time_used_seconds);
                apply_budget_limit_if_reached(&mut state);
            }
        }
        GoalEvent::Completed => {
            state.status = Some(status::COMPLETE.to_string());
        }
        GoalEvent::Cleared => {
            state = GoalState::default();
        }
    }
    state
}

/// Fold an entire goal event log into the current goal state.
///
/// Parity: legacy reconstructs goal state by folding the persisted event log in
/// `latest_thread_goal_from_events` (`browser-use-core/src/goals.rs:28-87`).
pub fn replay(events: &[GoalEvent]) -> GoalState {
    events.iter().fold(GoalState::default(), reduce)
}
