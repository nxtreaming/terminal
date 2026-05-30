//! Goal steering: the goal-context message + budget-threshold steering events.
//!
//! Two responsibilities, both pure:
//!
//! 1. [`render_goal_context_message`] — render the active goal as a
//!    `_CONTEXT_MESSAGE_NAME`-tagged context message. The ENVELOPE matches the
//!    legacy `goal_context_message` wire shape verbatim
//!    (`browser-use-core/src/lib.rs:9796-9805`):
//!    `{role:"user", name:"goal_context", content:[{type:"input_text", text}]}`.
//!    The name tag is
//!    [`crate::context::constants::GOAL_CONTEXT_MESSAGE_NAME`] (`"goal_context"`).
//!
//!    PARITY DEBT — the BODY: legacy fills the body with the large
//!    `GOAL_CONTINUATION_PROMPT_TEMPLATE` (`constants.rs:48-98`) rendered by
//!    `goal_continuation_prompt` (`goals.rs:442-466`) and wrapped in
//!    `<goal_context>…</goal_context>` (`lib.rs:9829-9831`). This WP renders a
//!    compact, deterministic summary instead (objective + status + budget
//!    usage); porting the full steering prompt verbatim is a later integration
//!    WP. The envelope/name are parity; the body is a documented simplification.
//!
//! 2. [`steering_events`] — diff two goal snapshots and emit [`PendingEvent`]s
//!    for the crossings the turn loop steers on: a goal becoming active, and the
//!    budget crossing the warn / exhaust thresholds. The exhaust event reuses the
//!    legacy budget-limit steering event type
//!    `GOAL_BUDGET_LIMIT_STEERING_EVENT = "goal.budget_limit_steering_requested"`
//!    (`constants.rs:129`), emitted in legacy by
//!    `budget_limited_goal_context_message_if_needed` (`goals.rs:409-440`) with a
//!    `{goal_id, reason:"token_budget_reached"}` payload (`goals.rs:432-437`).
//!
//! Crossings fire EXACTLY ONCE: an event is emitted only when the level on the
//! `prev` snapshot was below the level on the `next` snapshot, so re-polling at
//! the same level is silent.

use serde_json::json;
use serde_json::Value;

use crate::context::constants::GOAL_CONTEXT_MESSAGE_NAME;
use crate::events::PendingEvent;

use super::budget::BudgetLevel;
use super::budget::BudgetState;
use super::state::GoalState;

/// Goal lifecycle event name (legacy `GOAL_CREATED_EVENT`,
/// `browser-use-core/src/constants.rs:126`).
pub const GOAL_SET_EVENT: &str = "goal.created";
/// Soft budget-warning steering event. Local name; the legacy path has no soft
/// warn event (see [`super::budget::DEFAULT_WARN_FRACTION`]).
pub const GOAL_BUDGET_WARNING_EVENT: &str = "goal.budget_warning";
/// Hard budget-limit steering event.
///
/// Parity: legacy `GOAL_BUDGET_LIMIT_STEERING_EVENT`
/// (`browser-use-core/src/constants.rs:129`).
pub const GOAL_BUDGET_LIMIT_STEERING_EVENT: &str = "goal.budget_limit_steering_requested";

/// Render the active goal as a context message, or `None` when no goal is
/// active.
///
/// Envelope parity: legacy `goal_context_message`
/// (`browser-use-core/src/lib.rs:9796-9805`) and the agent's own
/// `context::inject::build_context_message` (`context/inject.rs:262-270`):
/// `{role:"user", name:"goal_context", content:[{type:"input_text", text}]}` —
/// note there is NO `"type":"message"` key, matching both. The body here is a
/// compact, deterministic summary (objective + status + budget usage) — see the
/// module-header PARITY DEBT note; the full `GOAL_CONTINUATION_PROMPT_TEMPLATE`
/// rendering is deferred to a later integration WP.
pub fn render_goal_context_message(state: &GoalState) -> Option<Value> {
    if !state.is_active() {
        return None;
    }
    let text = state.text.clone()?;
    let remaining = state
        .token_budget
        .map(|budget| budget.saturating_sub(state.tokens_used).max(0));
    let mut body = format!("Active thread goal:\n{text}");
    if let Some(status) = state.status.as_deref() {
        body.push_str(&format!("\n\nStatus: {status}"));
    }
    if let Some(budget) = state.token_budget {
        body.push_str(&format!(
            "\n\nToken budget: {budget}\nTokens used: {used}",
            used = state.tokens_used
        ));
        if let Some(remaining) = remaining {
            body.push_str(&format!("\nTokens remaining: {remaining}"));
        }
    }
    Some(json!({
        "role": "user",
        "name": GOAL_CONTEXT_MESSAGE_NAME,
        "content": [{
            "type": "input_text",
            "text": body,
        }],
    }))
}

/// Payload for a budget-limit (exhausted) steering event.
///
/// Parity: legacy emits `GOAL_BUDGET_LIMIT_STEERING_EVENT` with
/// `{goal_id, turn_idx, reason:"token_budget_reached"}`
/// (`browser-use-core/src/goals.rs:432-437`). We carry `goal_id` and
/// `reason:"token_budget_reached"` and add `tokens_used`/`token_budget` for the
/// consumer (there is no `turn_idx` at this seam).
fn budget_limit_payload(state: &GoalState) -> Value {
    json!({
        "type": GOAL_BUDGET_LIMIT_STEERING_EVENT,
        "reason": "token_budget_reached",
        "goal_id": state.goal_id,
        "tokens_used": state.tokens_used,
        "token_budget": state.token_budget,
    })
}

/// Payload for a soft budget-warning steering event.
fn budget_warning_payload(state: &GoalState, warn_threshold: i64) -> Value {
    json!({
        "type": GOAL_BUDGET_WARNING_EVENT,
        "reason": "token_budget_warning",
        "goal_id": state.goal_id,
        "tokens_used": state.tokens_used,
        "token_budget": state.token_budget,
        "warn_threshold": warn_threshold,
    })
}

/// Payload for a goal-set steering event (a goal became active).
fn goal_set_payload(state: &GoalState) -> Value {
    json!({
        "type": GOAL_SET_EVENT,
        "goal_id": state.goal_id,
        "text": state.text,
        "status": state.status,
        "token_budget": state.token_budget,
    })
}

/// Compute the steering events to emit for a `prev -> next` goal transition.
///
/// `session_id` is stamped on each [`PendingEvent`]. `prev_budget` / `budget`
/// are the budget snapshots that align with `prev_state` / `next_state`; only
/// the budget level is consulted, so the caller can pass freshly-derived
/// snapshots.
///
/// Fires:
///   * a [`GOAL_SET_EVENT`] when a goal transitions from inactive to active;
///   * a [`GOAL_BUDGET_WARNING_EVENT`] when the budget level rises into `Warn`;
///   * a [`GOAL_BUDGET_LIMIT_STEERING_EVENT`] when it rises into `Exhausted`.
///
/// Each crossing fires at most once per call and only when the level strictly
/// increased, so steady-state re-polls emit nothing.
pub fn steering_events(
    session_id: &str,
    prev_state: &GoalState,
    next_state: &GoalState,
    prev_budget: &BudgetState,
    budget: &BudgetState,
) -> Vec<PendingEvent> {
    let mut out = Vec::new();

    // Goal became active.
    if !prev_state.is_active() && next_state.is_active() {
        out.push(PendingEvent::new(
            session_id,
            GOAL_SET_EVENT,
            goal_set_payload(next_state),
        ));
    }

    // Budget level crossings (only when the goal is active so a cleared goal
    // does not fire stale budget steering).
    if next_state.is_active() {
        let prev_level = prev_budget.level();
        let next_level = budget.level();
        match (prev_level, next_level) {
            // Newly warning (and not already exhausted).
            (BudgetLevel::Ok, BudgetLevel::Warn) => {
                let threshold = super::budget::warn_threshold(
                    budget.max().unwrap_or(0),
                    super::budget::DEFAULT_WARN_FRACTION,
                );
                out.push(PendingEvent::new(
                    session_id,
                    GOAL_BUDGET_WARNING_EVENT,
                    budget_warning_payload(next_state, threshold),
                ));
            }
            // Newly exhausted (from below the hard limit, whether or not we
            // observed the warn band in between).
            (BudgetLevel::Ok | BudgetLevel::Warn, BudgetLevel::Exhausted) => {
                out.push(PendingEvent::new(
                    session_id,
                    GOAL_BUDGET_LIMIT_STEERING_EVENT,
                    budget_limit_payload(next_state),
                ));
            }
            _ => {}
        }
    }

    out
}
