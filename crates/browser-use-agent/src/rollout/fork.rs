//! By-turn-boundary fork of a rollout event log.
//!
//! This module FIXES the two debts logged in
//! [`crate::session::resume::fork_history_from_events`]
//! (`session/resume.rs:17-35`):
//!
//! 1. `ForkMode::LastN(n)` previously truncated by reconstructed-MESSAGE count
//!    (`full[full.len()-n..]`). Per codex / legacy fork parity it must keep the
//!    last `n` fork turns (turn boundary), not the last `n` messages.
//! 2. `ForkMode::Summary` previously aliased `All` (`ForkMode::All |
//!    ForkMode::Summary => full`). Per the WP brief it must be a DISTINCT
//!    behavior. Legacy has no real summary-fork (its doc says Summary "is
//!    treated as All until a summary checkpoint is wired in"), so — per the
//!    brief's instruction "if genuinely unsupported, make it an explicit
//!    distinct variant behavior + flag" — Summary here collapses all-but-the-
//!    last-turn into an explicit [`SummaryPlaceholder`] and carries only the
//!    most recent turn forward. This mirrors the SHAPE of codex's
//!    fork-with-compaction (`rollout-trace/src/compaction.rs`
//!    `CompactionCheckpointTracePayload` = input_history collapsed,
//!    replacement_history carried).
//!
//! Turn-counting parity (cited):
//! - legacy `browser-use-core/src/rollback.rs::rollback_last_n_user_turns:73`
//!   counts REAL user-event turns (`is_real_user_event` = `session.input` /
//!   `session.followup`, rollback.rs:114-119), slicing on a turn boundary, NOT
//!   on raw message/event count.
//! - codex `thread_rollout_truncation.rs::truncate_rollout_to_last_n_fork_turns`
//!   keeps the last `n` fork-turn boundaries. A fork boundary is a normal user
//!   input/followup or an inter-agent message with `trigger_turn = true`.

use browser_use_protocol::EventRecord;
use serde_json::Value;

use crate::session::ForkMode;

/// A summary placeholder marking collapsed pre-fork history.
///
/// Mirrors the SHAPE of codex's compaction checkpoint payload
/// (`codex-rs/rollout-trace/src/compaction.rs:83-87`
/// `CompactionCheckpointTracePayload { input_history, replacement_history }`):
/// `collapsed` is how many pre-fork events were dropped (the "input history"),
/// `summary` is the placeholder text standing in for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPlaceholder {
    /// Number of pre-fork events collapsed into this placeholder.
    pub collapsed: usize,
    /// Human-readable summary standing in for the collapsed history.
    pub summary: String,
}

/// The result of forking an event log by turn boundary.
///
/// (`PartialEq` only — carried [`EventRecord`]s hold a non-`Eq`
/// `serde_json::Value` payload.)
#[derive(Debug, Clone, PartialEq)]
pub struct ForkOutcome {
    /// The events carried into the child session, in order.
    pub carried: Vec<EventRecord>,
    /// For [`ForkMode::Summary`], the placeholder describing the collapsed
    /// pre-fork history. `None` for every other mode (this is the explicit flag
    /// distinguishing Summary from All).
    pub summary: Option<SummaryPlaceholder>,
}

/// Default summary text used when collapsing pre-fork history.
const DEFAULT_FORK_SUMMARY: &str = "[summary of earlier conversation]";

/// Collect the seqs of fork-turn events, in order.
///
/// Codex counts normal user inputs and inter-agent messages that set
/// `trigger_turn = true` as fork-turn boundaries.
fn fork_turn_seqs(events: &[EventRecord]) -> Vec<i64> {
    effective_events_after_rollbacks(events)
        .iter()
        .filter(|event| is_fork_turn_boundary_event(event))
        .map(|event| event.seq)
        .collect()
}

fn effective_events_after_rollbacks(events: &[EventRecord]) -> Vec<EventRecord> {
    let mut checkpoint_messages = Vec::new();
    crate::session::rollback_filtered_events_after_for_fork(events, 0, &mut checkpoint_messages)
        .into_iter()
        .cloned()
        .collect()
}

fn is_fork_turn_boundary_event(event: &EventRecord) -> bool {
    if crate::session::is_real_user_event(event) {
        return true;
    }
    matches!(
        event.event_type.as_str(),
        "agent.message" | "agent.mailbox_input"
    ) && event
        .payload
        .get("content")
        .and_then(Value::as_str)
        .is_some()
        && event
            .payload
            .get("trigger_turn")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// The seq at which the last `n` user turns begin, or `None` if there are no
/// user turns (or `n == 0`). Keeping events with `seq >= boundary` keeps exactly
/// the last `n` user turns.
///
/// Codex parity: when `n` exceeds the number of fork turns, keep the full rollout,
/// including system/developer/context events before the first turn.
fn last_n_user_turns_start_seq(events: &[EventRecord], n: usize) -> Option<i64> {
    if n == 0 {
        return None;
    }
    let seqs = fork_turn_seqs(events);
    if seqs.is_empty() {
        return None;
    }
    if n >= seqs.len() {
        return Some(0);
    }
    let idx = seqs.len().saturating_sub(n);
    Some(seqs[idx])
}

/// Fork an event log into a child according to `mode`, by TURN boundary.
///
/// Returns the raw carried [`EventRecord`]s (plus, for `Summary`, the collapse
/// placeholder) so the result can be fed straight back through
/// [`crate::session::resume`] reducers, keeping replay-correctness intact.
pub fn fork_events_by_turn(events: &[EventRecord], mode: &ForkMode) -> ForkOutcome {
    match mode {
        ForkMode::None => ForkOutcome {
            carried: Vec::new(),
            summary: None,
        },
        ForkMode::All => ForkOutcome {
            carried: effective_events_after_rollbacks(events),
            summary: None,
        },
        ForkMode::LastN(n) => {
            // DEBT FIX #1: keep the last `n` fork turns, not the last `n`
            // messages. Legacy parity: rollback_last_n_user_turns:73.
            let effective = effective_events_after_rollbacks(events);
            match last_n_user_turns_start_seq(events, *n) {
                Some(start_seq) => ForkOutcome {
                    carried: effective
                        .iter()
                        .filter(|event| event.seq >= start_seq)
                        .cloned()
                        .collect(),
                    summary: None,
                },
                // Codex keeps the full effective rollout when there are no fork
                // boundaries but the caller asked for a positive LastN budget.
                // Only an explicit zero budget carries nothing.
                None if *n > 0 => ForkOutcome {
                    carried: effective,
                    summary: None,
                },
                None => ForkOutcome {
                    carried: Vec::new(),
                    summary: None,
                },
            }
        }
        ForkMode::Summary => {
            // DEBT FIX #2: Summary is DISTINCT from All. Collapse all-but-the-
            // last-turn into a placeholder; carry the most recent turn forward.
            summary_fork(
                &effective_events_after_rollbacks(events),
                DEFAULT_FORK_SUMMARY,
            )
        }
    }
}

/// Collapse all-but-the-last-user-turn into a summary placeholder; carry the
/// most recent turn (the final real user event and everything after it) forward.
///
/// Shape parity: codex `compaction.rs` keeps `input_history` (collapsed) vs
/// `replacement_history` (carried) separate; here the carried slice is the last
/// turn and the placeholder records the collapsed-prefix size.
fn summary_fork(events: &[EventRecord], summary: &str) -> ForkOutcome {
    let seqs = fork_turn_seqs(events);
    match seqs.last() {
        Some(&last_user_seq) => {
            let split = events
                .iter()
                .position(|event| event.seq == last_user_seq)
                .unwrap_or(0);
            let (collapsed, carried) = events.split_at(split);
            ForkOutcome {
                carried: carried.to_vec(),
                summary: Some(SummaryPlaceholder {
                    collapsed: collapsed.len(),
                    summary: summary.to_string(),
                }),
            }
        }
        // No real user turns: carry nothing, but still distinct from All — an
        // empty carry plus an explicit placeholder (collapsed == events.len()).
        None => ForkOutcome {
            carried: Vec::new(),
            summary: Some(SummaryPlaceholder {
                collapsed: events.len(),
                summary: summary.to_string(),
            }),
        },
    }
}
