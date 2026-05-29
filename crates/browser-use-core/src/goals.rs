//! Goal state, budget accounting, and steering prompts extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use anyhow::Result;
use browser_use_protocol::EventRecord;
use browser_use_store::{now_ms, Store};
use serde_json::Value;

use crate::constants::*;
use crate::{
    add_codex_token_usage, budget_limited_goal_context_message_from_events,
    empty_codex_token_usage, escape_xml_text, json_payload_i64,
    latest_codex_total_token_usage_from_events, subtract_codex_token_usage,
};

#[derive(Clone, Debug)]
pub(crate) struct ThreadGoalSnapshot {
    pub(crate) goal_id: String,
    pub(crate) objective: String,
    pub(crate) status: String,
    pub(crate) token_budget: Option<i64>,
    pub(crate) baseline_total_token_usage: Value,
    pub(crate) created_at_ms: Option<i64>,
    pub(crate) updated_at_ms: Option<i64>,
}

pub(crate) fn latest_thread_goal_from_events(events: &[EventRecord]) -> Option<ThreadGoalSnapshot> {
    let mut goal = None::<ThreadGoalSnapshot>;
    for event in events {
        match event.event_type.as_str() {
            GOAL_CREATED_EVENT => {
                let objective = event
                    .payload
                    .get("objective")
                    .and_then(Value::as_str)?
                    .trim()
                    .to_string();
                if objective.is_empty() {
                    continue;
                }
                goal = Some(ThreadGoalSnapshot {
                    goal_id: event
                        .payload
                        .get("goal_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| event.id.clone()),
                    objective,
                    status: event
                        .payload
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("active")
                        .to_string(),
                    token_budget: event.payload.get("token_budget").and_then(Value::as_i64),
                    baseline_total_token_usage: event
                        .payload
                        .get("baseline_total_token_usage")
                        .cloned()
                        .unwrap_or_else(empty_codex_token_usage),
                    created_at_ms: event.payload.get("created_at_ms").and_then(Value::as_i64),
                    updated_at_ms: event
                        .payload
                        .get("updated_at_ms")
                        .and_then(Value::as_i64)
                        .or_else(|| event.payload.get("created_at_ms").and_then(Value::as_i64)),
                });
            }
            GOAL_UPDATED_EVENT => {
                let Some(current) = goal.as_mut() else {
                    continue;
                };
                if let Some(status) = event.payload.get("status").and_then(Value::as_str) {
                    current.status = status.to_string();
                }
                if let Some(updated_at_ms) =
                    event.payload.get("updated_at_ms").and_then(Value::as_i64)
                {
                    current.updated_at_ms = Some(updated_at_ms);
                }
            }
            _ => {}
        }
    }
    goal
}

fn goal_created_event_ts_ms(events: &[EventRecord], goal_id: &str) -> Option<i64> {
    events.iter().rev().find_map(|event| {
        (event.event_type == GOAL_CREATED_EVENT)
            .then(|| {
                event
                    .payload
                    .get("goal_id")
                    .and_then(Value::as_str)
                    .map(|payload_goal_id| payload_goal_id == goal_id)
                    .unwrap_or(event.id == goal_id)
            })
            .filter(|matches| *matches)
            .map(|_| event.ts_ms)
    })
}

fn goal_accounting_matches_goal(event: &EventRecord, goal_id: &str) -> bool {
    event.event_type == GOAL_ACCOUNTING_EVENT
        && event.payload.get("goal_id").and_then(Value::as_str) == Some(goal_id)
}

fn goal_accounted_usage_from_events(events: &[EventRecord], goal_id: &str) -> (Value, i64, bool) {
    let mut token_usage = empty_codex_token_usage();
    let mut time_used_seconds = 0_i64;
    let mut saw_accounting = false;
    for event in events
        .iter()
        .filter(|event| goal_accounting_matches_goal(event, goal_id))
    {
        saw_accounting = true;
        if let Some(delta) = event.payload.get("token_usage_delta") {
            token_usage = add_codex_token_usage(&token_usage, delta);
        }
        time_used_seconds = time_used_seconds.saturating_add(
            event
                .payload
                .get("time_delta_seconds")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0),
        );
    }
    (token_usage, time_used_seconds, saw_accounting)
}

fn latest_goal_accounting_baseline(
    events: &[EventRecord],
    goal: &ThreadGoalSnapshot,
) -> (Value, Option<i64>) {
    for event in events.iter().rev() {
        if !goal_accounting_matches_goal(event, &goal.goal_id) {
            continue;
        }
        let total_usage = event
            .payload
            .get("total_token_usage")
            .cloned()
            .unwrap_or_else(|| {
                let (accounted_usage, _, _) =
                    goal_accounted_usage_from_events(events, &goal.goal_id);
                add_codex_token_usage(&goal.baseline_total_token_usage, &accounted_usage)
            });
        let accounted_at_ms = event
            .payload
            .get("accounted_at_ms")
            .and_then(Value::as_i64)
            .or(Some(event.ts_ms));
        return (total_usage, accounted_at_ms);
    }
    (
        goal.baseline_total_token_usage.clone(),
        goal.created_at_ms
            .or_else(|| goal_created_event_ts_ms(events, &goal.goal_id)),
    )
}

fn goal_usage_payload(events: &[EventRecord], goal: &ThreadGoalSnapshot) -> Value {
    let current_total_usage = latest_codex_total_token_usage_from_events(events);
    let fallback_token_usage =
        subtract_codex_token_usage(&current_total_usage, &goal.baseline_total_token_usage);
    let (accounted_token_usage, time_used_seconds, saw_accounting) =
        goal_accounted_usage_from_events(events, &goal.goal_id);
    let token_usage = if saw_accounting {
        accounted_token_usage
    } else {
        fallback_token_usage
    };
    let tokens_used = goal_token_delta_for_usage(&token_usage);
    let remaining_tokens = goal
        .token_budget
        .map(|budget| budget.saturating_sub(tokens_used).max(0));
    let elapsed_time_ms = time_used_seconds.saturating_mul(1000);
    serde_json::json!({
        "tokens_used": tokens_used,
        "token_usage": token_usage,
        "baseline_total_token_usage": goal.baseline_total_token_usage,
        "current_total_token_usage": current_total_usage,
        "token_budget": goal.token_budget,
        "remaining_tokens": remaining_tokens,
        "time_used_seconds": time_used_seconds,
        "elapsed_time_ms": elapsed_time_ms,
    })
}

fn goal_time_used_seconds(usage: &Value) -> i64 {
    usage
        .get("elapsed_time_ms")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .saturating_div(1000)
}

pub(crate) fn append_goal_progress_accounting(
    store: &Store,
    session_id: &str,
    turn_idx: usize,
    reason: &str,
    turn_started_at_ms: i64,
    account_tokens: bool,
) -> Result<()> {
    let events = store.events_for_session(session_id)?;
    let Some(goal) = latest_thread_goal_from_events(&events) else {
        return Ok(());
    };
    if !matches!(goal.status.as_str(), "active" | "budget_limited") {
        return Ok(());
    }
    let current_total_usage = latest_codex_total_token_usage_from_events(&events);
    let (last_accounted_usage, last_accounted_at_ms) =
        latest_goal_accounting_baseline(&events, &goal);
    let token_usage_delta = if account_tokens {
        subtract_codex_token_usage(&current_total_usage, &last_accounted_usage)
    } else {
        empty_codex_token_usage()
    };
    let token_delta = goal_token_delta_for_usage(&token_usage_delta);
    let now = now_ms();
    let time_baseline = last_accounted_at_ms
        .unwrap_or(turn_started_at_ms)
        .max(turn_started_at_ms);
    let time_delta_seconds = now.saturating_sub(time_baseline).max(0) / 1000;
    if token_delta <= 0 && time_delta_seconds <= 0 {
        return Ok(());
    }
    store.append_event(
        session_id,
        GOAL_ACCOUNTING_EVENT,
        serde_json::json!({
            "goal_id": goal.goal_id,
            "turn_idx": turn_idx,
            "reason": reason,
            "token_delta": token_delta,
            "token_usage_delta": token_usage_delta,
            "total_token_usage": current_total_usage,
            "time_delta_seconds": time_delta_seconds,
            "accounted_at_ms": now,
        }),
    )?;
    Ok(())
}

fn goal_effective_status(goal: &ThreadGoalSnapshot, usage: &Value) -> String {
    if goal.status == "active"
        && goal.token_budget.is_some_and(|budget| {
            usage
                .get("tokens_used")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                >= budget
        })
    {
        "budget_limited".to_string()
    } else {
        goal.status.clone()
    }
}

fn codex_goal_payload(
    session_id: &str,
    goal: &ThreadGoalSnapshot,
    usage: &Value,
    status: &str,
) -> Value {
    let created_at = goal.created_at_ms.map(|ms| ms.saturating_div(1000));
    let updated_at = goal.updated_at_ms.map(|ms| ms.saturating_div(1000));
    serde_json::json!({
        "threadId": session_id,
        "objective": goal.objective,
        "status": status,
        "tokenBudget": goal.token_budget,
        "tokensUsed": usage.get("tokens_used").and_then(Value::as_i64).unwrap_or(0),
        "timeUsedSeconds": goal_time_used_seconds(usage),
        "createdAt": created_at,
        "updatedAt": updated_at,
    })
}

fn completion_budget_report_for_goal(
    goal: &ThreadGoalSnapshot,
    usage: &Value,
    status: &str,
    include: bool,
) -> Value {
    if include
        && status == "complete"
        && (goal.token_budget.is_some() || goal_time_used_seconds(usage) > 0)
    {
        Value::String(GOAL_COMPLETION_BUDGET_REPORT.to_string())
    } else {
        Value::Null
    }
}

pub(crate) fn goal_snapshot_payload(
    session_id: &str,
    events: &[EventRecord],
    goal: &ThreadGoalSnapshot,
    include_completion_budget_report: bool,
) -> Value {
    let usage = goal_usage_payload(events, goal);
    let status = goal_effective_status(goal, &usage);
    let completion_budget_report =
        completion_budget_report_for_goal(goal, &usage, &status, include_completion_budget_report);
    let remaining_tokens = usage
        .get("remaining_tokens")
        .cloned()
        .unwrap_or(Value::Null);
    let codex_goal = codex_goal_payload(session_id, goal, &usage, &status);
    serde_json::json!({
        "goal_id": goal.goal_id,
        "objective": goal.objective,
        "status": status,
        "created_at_ms": goal.created_at_ms,
        "updated_at_ms": goal.updated_at_ms,
        "usage": usage,
        "goal": codex_goal,
        "remainingTokens": remaining_tokens,
        "completionBudgetReport": completion_budget_report,
    })
}

fn goal_token_delta_for_usage(usage: &Value) -> i64 {
    json_payload_i64(usage, "input_tokens")
        .saturating_sub(json_payload_i64(usage, "cached_input_tokens"))
        .saturating_add(json_payload_i64(usage, "output_tokens").max(0))
        .max(0)
}

pub(crate) fn maybe_mark_goal_budget_limited(
    store: &Store,
    session_id: &str,
    turn_idx: usize,
    reason: &str,
) -> Result<()> {
    let events = store.events_for_session(session_id)?;
    let Some(goal) = latest_thread_goal_from_events(&events) else {
        return Ok(());
    };
    if goal.status != "active" {
        return Ok(());
    }
    let Some(token_budget) = goal.token_budget else {
        return Ok(());
    };
    let usage = goal_usage_payload(&events, &goal);
    let tokens_used = usage
        .get("tokens_used")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if tokens_used < token_budget {
        return Ok(());
    }
    store.append_event(
        session_id,
        GOAL_UPDATED_EVENT,
        serde_json::json!({
            "goal_id": goal.goal_id,
            "status": "budget_limited",
            "updated_at_ms": now_ms(),
            "tool_call_id": Value::Null,
            "turn_idx": turn_idx,
            "reason": reason,
            "tokens_used": tokens_used,
            "token_budget": token_budget,
        }),
    )?;
    Ok(())
}

pub(crate) fn maybe_mark_goal_usage_limited(
    store: &Store,
    session_id: &str,
    turn_idx: usize,
    reason: &str,
) -> Result<()> {
    let events = store.events_for_session(session_id)?;
    let Some(goal) = latest_thread_goal_from_events(&events) else {
        return Ok(());
    };
    if !matches!(goal.status.as_str(), "active" | "budget_limited") {
        return Ok(());
    }
    let usage = goal_usage_payload(&events, &goal);
    store.append_event(
        session_id,
        GOAL_UPDATED_EVENT,
        serde_json::json!({
            "goal_id": goal.goal_id,
            "status": "usage_limited",
            "updated_at_ms": now_ms(),
            "tool_call_id": Value::Null,
            "turn_idx": turn_idx,
            "reason": reason,
            "tokens_used": usage.get("tokens_used").cloned().unwrap_or(Value::Null),
            "token_budget": goal.token_budget,
        }),
    )?;
    Ok(())
}

pub(crate) fn budget_limited_goal_context_message_if_needed(
    store: &Store,
    session_id: &str,
    turn_idx: usize,
) -> Result<Option<Value>> {
    let events = store.events_for_session(session_id)?;
    let Some(goal) = latest_thread_goal_from_events(&events) else {
        return Ok(None);
    };
    if goal.status != "budget_limited" {
        return Ok(None);
    }
    if events.iter().any(|event| {
        event.event_type == GOAL_BUDGET_LIMIT_STEERING_EVENT
            && event.payload.get("goal_id").and_then(Value::as_str) == Some(goal.goal_id.as_str())
    }) {
        return Ok(None);
    }
    let Some(message) = budget_limited_goal_context_message_from_events(&events) else {
        return Ok(None);
    };
    store.append_event(
        session_id,
        GOAL_BUDGET_LIMIT_STEERING_EVENT,
        serde_json::json!({
            "goal_id": goal.goal_id,
            "turn_idx": turn_idx,
            "reason": "token_budget_reached",
        }),
    )?;
    Ok(Some(message))
}

pub(crate) fn goal_continuation_prompt(
    events: &[EventRecord],
    goal: &ThreadGoalSnapshot,
) -> String {
    let usage = goal_usage_payload(events, goal);
    let tokens_used = usage
        .get("tokens_used")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .to_string();
    let token_budget = goal
        .token_budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "none".to_string());
    let remaining_tokens = usage
        .get("remaining_tokens")
        .and_then(Value::as_i64)
        .map(|remaining| remaining.to_string())
        .unwrap_or_else(|| "unbounded".to_string());
    GOAL_CONTINUATION_PROMPT_TEMPLATE
        .replace("{objective}", &escape_xml_text(&goal.objective))
        .replace("{tokens_used}", &tokens_used)
        .replace("{token_budget}", &token_budget)
        .replace("{remaining_tokens}", &remaining_tokens)
}

pub(crate) fn goal_budget_limit_prompt(
    events: &[EventRecord],
    goal: &ThreadGoalSnapshot,
) -> String {
    let usage = goal_usage_payload(events, goal);
    let tokens_used = usage
        .get("tokens_used")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .to_string();
    let token_budget = goal
        .token_budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "none".to_string());
    let time_used_seconds = goal_time_used_seconds(&usage).to_string();
    GOAL_BUDGET_LIMIT_PROMPT_TEMPLATE
        .replace("{objective}", &escape_xml_text(&goal.objective))
        .replace("{time_used_seconds}", &time_used_seconds)
        .replace("{tokens_used}", &tokens_used)
        .replace("{token_budget}", &token_budget)
}
