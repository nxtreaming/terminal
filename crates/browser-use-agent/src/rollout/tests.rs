//! Network-free tests for the rollout subsystem.
//!
//! Truncation/fork tests are pure over [`EventRecord`] fixtures (same fixture
//! style as `session/reconstruct_tests.rs`). The archive test uses a real
//! `browser_use_store::Store` on a `tempfile::tempdir()` (local SQLite, not a
//! network) plus a fake archiver, and asserts the durable record was *written*
//! (it does not hot-read the live engine path).

use std::sync::{Arc, Mutex};

use browser_use_protocol::EventRecord;
use serde_json::{json, Value};

use crate::rollout::archive::{ArchiveError, RolloutArchiver, RolloutBundle, StoreRolloutArchiver};
use crate::rollout::fork::{fork_events_by_turn, SummaryPlaceholder};
use crate::rollout::truncation::{truncate_rollout_if_needed, DEFAULT_THREAD_ROLLOUT_MAX_BYTES};
use crate::rollout::{RolloutManager, ROLLOUT_ARCHIVE_EVENT_TYPE};
use crate::session::resume::history_from_events;
use crate::session::ForkMode;

// ---- helpers ----------------------------------------------------------------

fn event(seq: i64, ty: &str, payload: Value) -> EventRecord {
    EventRecord {
        seq,
        id: format!("e{seq}"),
        session_id: "s1".to_string(),
        ts_ms: seq,
        event_type: ty.to_string(),
        payload,
    }
}

fn user(seq: i64, text: &str) -> EventRecord {
    event(seq, "session.input", json!({ "text": text }))
}

fn assistant(seq: i64, text: &str) -> EventRecord {
    event(
        seq,
        "model.response.output_item",
        json!({
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }],
            }
        }),
    )
}

fn tool_call(seq: i64, call_id: &str, name: &str) -> EventRecord {
    event(
        seq,
        "model.tool_call",
        json!({ "id": call_id, "name": name, "arguments": {} }),
    )
}

fn tool_output(seq: i64, call_id: &str, name: &str, out: &str) -> EventRecord {
    event(
        seq,
        "tool.output",
        json!({ "tool_call_id": call_id, "name": name, "output": out }),
    )
}

/// A workspace-context event = the de-facto session-start header (anchored
/// before the first user turn). Not a real user turn.
fn workspace_context(seq: i64, before_seq: i64) -> EventRecord {
    event(
        seq,
        "workspace.context",
        json!({
            "kind": "environment_context",
            "before_seq": before_seq,
            "content": "<environment_context>cwd=/tmp</environment_context>",
        }),
    )
}

fn agent_message(seq: i64, content: &str, trigger_turn: bool) -> EventRecord {
    event(
        seq,
        "agent.message",
        json!({
            "source": "/root/a",
            "target": "/root/b",
            "content": content,
            "trigger_turn": trigger_turn,
        }),
    )
}

fn rollback(seq: i64, num_turns: usize) -> EventRecord {
    event(seq, "session.rollback", json!({ "num_turns": num_turns }))
}

/// Four user turns, each with an assistant reply (8 events, seqs 1..=8).
fn four_turn_log() -> Vec<EventRecord> {
    vec![
        user(1, "turn0"),
        assistant(2, "reply0"),
        user(3, "turn1"),
        assistant(4, "reply1"),
        user(5, "turn2"),
        assistant(6, "reply2"),
        user(7, "turn3"),
        assistant(8, "reply3"),
    ]
}

// ---- truncation -------------------------------------------------------------

#[test]
fn truncation_threshold_matches_codex() {
    // Codex parity: conversation_history.rs:11 -> MAX_ROLLOUT_BYTES_BEFORE_TRUNCATION = 5 MiB.
    assert_eq!(DEFAULT_THREAD_ROLLOUT_MAX_BYTES, 5 * 1024 * 1024);
}

#[test]
fn truncation_under_budget_is_untouched() {
    let log = four_turn_log();
    let outcome = truncate_rollout_if_needed(&log, DEFAULT_THREAD_ROLLOUT_MAX_BYTES);
    assert!(!outcome.did_truncate());
    assert_eq!(outcome.kept, log);
    assert!(outcome.archived.is_empty());
}

#[test]
fn truncation_over_budget_drops_oldest_and_keeps_recent() {
    // Build a log far over a tiny budget. Codex drops oldest-until-fit, always
    // keeping >= the most recent event.
    let big = "x".repeat(400);
    let mut log = vec![workspace_context(1, 2)];
    for i in 0..20i64 {
        log.push(user(2 + i * 2, &format!("msg{i}-{big}")));
        log.push(assistant(3 + i * 2, &format!("rep{i}-{big}")));
    }
    let last = log.last().cloned().unwrap();

    let outcome = truncate_rollout_if_needed(&log, 1500);
    assert!(outcome.did_truncate());

    // Oldest events were archived (drop-oldest), most-recent is always kept.
    assert_eq!(outcome.kept.last(), Some(&last));
    // Archived is a strict prefix of the original log.
    assert_eq!(outcome.archived.as_slice(), &log[..outcome.archived.len()]);
    // Kept is the matching suffix; partition is lossless.
    assert_eq!(outcome.kept.len() + outcome.archived.len(), log.len());
    assert_eq!(outcome.kept.as_slice(), &log[outcome.archived.len()..]);
}

#[test]
fn truncation_keeps_last_event_even_if_alone_over_budget() {
    // A single huge event over budget is still kept (codex never drops the last
    // item: `index + 1 < len`).
    let huge = "z".repeat(10_000);
    let log = vec![user(1, &huge)];
    let outcome = truncate_rollout_if_needed(&log, 10);
    assert_eq!(outcome.kept, log);
    assert!(outcome.archived.is_empty());
}

#[test]
fn truncation_drops_minimum_needed_only() {
    // Each event ~ a few hundred bytes; budget allows roughly the tail. Assert
    // we drop the minimal oldest prefix (codex advances index only while over
    // budget).
    let body = "y".repeat(300);
    let log: Vec<EventRecord> = (1..=10i64)
        .map(|i| user(i, &format!("{i}-{body}")))
        .collect();
    let total: usize = log
        .iter()
        .map(|e| serde_json::to_string(e).unwrap().len())
        .sum();
    let one = serde_json::to_string(&log[0]).unwrap().len();
    // Budget = total minus 2.5 events: codex must drop exactly the oldest 3.
    let budget = total - one * 5 / 2;
    let outcome = truncate_rollout_if_needed(&log, budget);
    assert!(outcome.did_truncate());
    let kept_bytes: usize = outcome
        .kept
        .iter()
        .map(|e| serde_json::to_string(e).unwrap().len())
        .sum();
    assert!(kept_bytes <= budget, "kept must fit budget");
    // Dropping one fewer would exceed budget -> minimal drop.
    let kept_plus_one: usize = kept_bytes
        + serde_json::to_string(&outcome.archived.last().unwrap())
            .unwrap()
            .len();
    assert!(
        kept_plus_one > budget,
        "drop is minimal (kept+1 oldest would overflow)"
    );
}

// ---- replay-correctness after truncation ------------------------------------

#[test]
fn truncated_rollout_reduces_to_valid_history() {
    // A log with call/output pairs; truncate hard so a call may lose its output
    // (or an output lose its call). The kept slice must still reduce via
    // session::resume with NO orphan tool output (the reducer's
    // normalize_provider_messages reconciles dangling calls/outputs).
    let big = "p".repeat(120);
    let mut log = vec![workspace_context(1, 2)];
    let mut seq = 2i64;
    for i in 0..8i64 {
        log.push(user(seq, &format!("u{i}")));
        seq += 1;
        log.push(tool_call(seq, &format!("c{i}"), &format!("call{i}")));
        seq += 1;
        log.push(tool_output(
            seq,
            &format!("c{i}"),
            &format!("call{i}"),
            &format!("out{i}-{big}"),
        ));
        seq += 1;
        log.push(assistant(seq, &format!("a{i}")));
        seq += 1;
    }
    // Append a terminator so the reducer flushes any open turn.
    log.push(event(seq, "session.done", json!({})));

    let outcome = truncate_rollout_if_needed(&log, 800);
    assert!(outcome.did_truncate());

    let history = history_from_events(&outcome.kept);
    // No orphan tool output: every `tool`-role message must be immediately
    // preceded by an assistant message carrying its tool_call_id.
    for (idx, msg) in history.iter().enumerate() {
        if msg.get("role").and_then(Value::as_str) == Some("tool") {
            let call_id = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            assert!(idx > 0, "tool output is first message (orphan): {msg:?}");
            let prev = &history[idx - 1];
            let prev_has_call = prev
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| {
                    calls.iter().any(|c| {
                        c.get("id").and_then(Value::as_str) == Some(call_id)
                            || c.get("call_id").and_then(Value::as_str) == Some(call_id)
                    })
                });
            assert!(
                prev_has_call,
                "tool output {call_id} not preceded by its call (orphan); prev={prev:?}"
            );
        }
    }
}

// ---- fork by turn (DEBT FIX #1) ---------------------------------------------

#[test]
fn fork_lastn_is_by_turn_not_by_message() {
    let log = four_turn_log(); // 4 user turns (seqs 1,3,5,7), 8 events.

    // Keep last 2 USER TURNS => turns starting at seq 5 (turn2) and seq 7
    // (turn3): events with seq >= 5 -> [user5, asst6, user7, asst8] (4 events).
    let outcome = fork_events_by_turn(&log, &ForkMode::LastN(2));
    let carried_seqs: Vec<i64> = outcome.carried.iter().map(|e| e.seq).collect();
    assert_eq!(carried_seqs, vec![5, 6, 7, 8]);
    assert!(outcome.summary.is_none());

    // The DEBT: a naive last-2-EVENTS slice would keep only [seq7, seq8].
    let naive_last2: Vec<i64> = log.iter().rev().take(2).rev().map(|e| e.seq).collect();
    assert_eq!(naive_last2, vec![7, 8]);
    assert_ne!(
        carried_seqs, naive_last2,
        "by-turn fork must differ from naive last-2-events slice"
    );

    // Exactly 2 real user turns carried.
    let user_count = outcome
        .carried
        .iter()
        .filter(|e| crate::session::is_real_user_event(e))
        .count();
    assert_eq!(user_count, 2);
}

#[test]
fn fork_lastn_more_turns_than_exist_carries_all() {
    let log = four_turn_log();
    let outcome = fork_events_by_turn(&log, &ForkMode::LastN(99));
    let seqs: Vec<i64> = outcome.carried.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn fork_lastn_skips_non_user_events() {
    // A workspace.context (header) is NOT a turn boundary.
    let log = vec![
        workspace_context(1, 2),
        user(2, "real0"),
        assistant(3, "r0"),
        user(4, "real1"),
        assistant(5, "r1"),
    ];
    // Last 1 user turn => from seq 4 onward.
    let outcome = fork_events_by_turn(&log, &ForkMode::LastN(1));
    let seqs: Vec<i64> = outcome.carried.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![4, 5]);
}

#[test]
fn fork_lastn_without_turn_boundaries_keeps_effective_rollout() {
    let log = vec![workspace_context(1, 2), assistant(2, "preface")];
    let outcome = fork_events_by_turn(&log, &ForkMode::LastN(1));
    let seqs: Vec<i64> = outcome.carried.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2]);
}

#[test]
fn fork_lastn_counts_triggered_inter_agent_messages() {
    let log = vec![
        user(1, "root-turn"),
        assistant(2, "root-reply"),
        agent_message(3, "queued note", false),
        assistant(4, "ignored for boundary"),
        agent_message(5, "please continue", true),
        assistant(6, "child reply"),
    ];

    let outcome = fork_events_by_turn(&log, &ForkMode::LastN(1));
    let seqs: Vec<i64> = outcome.carried.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![5, 6]);
}

#[test]
fn fork_lastn_applies_rollback_markers_before_selecting_suffix() {
    let log = vec![
        user(1, "kept"),
        assistant(2, "kept reply"),
        user(3, "rolled back"),
        assistant(4, "stale reply"),
        rollback(5, 1),
        user(6, "new suffix"),
        assistant(7, "new reply"),
    ];

    let one = fork_events_by_turn(&log, &ForkMode::LastN(1));
    let one_seqs: Vec<i64> = one.carried.iter().map(|event| event.seq).collect();
    assert_eq!(one_seqs, vec![6, 7]);

    let two = fork_events_by_turn(&log, &ForkMode::LastN(2));
    let two_seqs: Vec<i64> = two.carried.iter().map(|event| event.seq).collect();
    assert_eq!(two_seqs, vec![1, 2, 6, 7]);
}

#[test]
fn fork_none_carries_nothing() {
    let log = four_turn_log();
    let outcome = fork_events_by_turn(&log, &ForkMode::None);
    assert!(outcome.carried.is_empty());
    assert!(outcome.summary.is_none());
}

#[test]
fn fork_all_carries_everything() {
    let log = four_turn_log();
    let outcome = fork_events_by_turn(&log, &ForkMode::All);
    assert_eq!(outcome.carried, log);
    assert!(outcome.summary.is_none());
}

// ---- fork Summary != All (DEBT FIX #2) --------------------------------------

#[test]
fn fork_summary_is_distinct_from_all() {
    let log = four_turn_log();

    let all = fork_events_by_turn(&log, &ForkMode::All);
    let summary = fork_events_by_turn(&log, &ForkMode::Summary);

    // Summary must NOT alias All.
    assert_ne!(all.carried, summary.carried);
    assert!(all.summary.is_none());
    assert!(summary.summary.is_some());

    // Summary collapses all-but-last-turn; carries only the most recent turn
    // (from last real user seq 7).
    let carried_seqs: Vec<i64> = summary.carried.iter().map(|e| e.seq).collect();
    assert_eq!(carried_seqs, vec![7, 8]);

    let placeholder: &SummaryPlaceholder = summary.summary.as_ref().unwrap();
    // Six events (seqs 1..=6) were collapsed.
    assert_eq!(placeholder.collapsed, 6);
    assert!(!placeholder.summary.is_empty());
}

// ---- archive (real Store + fake) --------------------------------------------

#[tokio::test]
async fn archive_writes_durable_record_to_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = browser_use_store::Store::open(dir.path()).expect("open store");
    // The events table has a FK on sessions(id); create the session first so the
    // archive rows have a valid parent (write-sink discipline still holds — we
    // only ever WRITE).
    let session = store.create_session(None, "/tmp").expect("create session");
    let session_id = session.id.clone();
    let events_before = store.events_for_session(&session_id).expect("events").len();

    let archiver = StoreRolloutArchiver::new(store);
    let bundle = RolloutBundle {
        session_id: session_id.clone(),
        archived_events: vec![user(1, "old0"), assistant(2, "oldr0")],
        kept_events: vec![user(3, "new0"), assistant(4, "newr0")],
    };
    assert!(bundle.did_archive());
    assert_eq!(bundle.total(), 4);

    let written = archiver.archive_rollout(&bundle).await.expect("archive");
    assert_eq!(written, 4);

    // Durability assertion: reopen the SAME db and verify exactly 4 archive
    // rows landed. This is a test-only durability probe over the write-sink, not
    // a hot-read in the engine path.
    drop(archiver);
    let probe = browser_use_store::Store::open(dir.path()).expect("reopen store");
    let all = probe.events_for_session(&session_id).expect("events");
    let archived: Vec<_> = all
        .iter()
        .filter(|e| e.event_type == ROLLOUT_ARCHIVE_EVENT_TYPE)
        .collect();
    assert_eq!(archived.len(), 4, "4 archive rows durably written");
    assert_eq!(all.len(), events_before + 4);
    // The archive payload carries partition + orig_seq + verbatim event.
    assert_eq!(
        archived[0].payload.get("partition").and_then(Value::as_str),
        Some("archived")
    );
    assert_eq!(
        archived[2].payload.get("partition").and_then(Value::as_str),
        Some("kept")
    );
    assert_eq!(
        archived[0].payload.get("orig_seq").and_then(Value::as_i64),
        Some(1)
    );
}

/// A fake archiver capturing bundles in memory.
#[derive(Clone, Default)]
struct FakeArchiver {
    captured: Arc<Mutex<Vec<RolloutBundle>>>,
}

impl RolloutArchiver for FakeArchiver {
    async fn archive_rollout(&self, bundle: &RolloutBundle) -> Result<usize, ArchiveError> {
        let mut g = self.captured.lock().unwrap();
        g.push(bundle.clone());
        Ok(bundle.total())
    }
}

#[tokio::test]
async fn manager_truncate_and_archive_uses_fake_seam() {
    // Build a log far over a tiny budget so truncation archives a prefix.
    let big = "q".repeat(300);
    let log: Vec<EventRecord> = (1..=15i64)
        .map(|i| user(i, &format!("m{i}-{big}")))
        .collect();

    let fake = FakeArchiver::default();
    let captured = Arc::clone(&fake.captured);
    let mgr = RolloutManager::with_max_bytes(fake, 1200);

    let (outcome, written) = mgr
        .truncate_and_archive("session-B", &log)
        .await
        .expect("truncate_and_archive");

    assert!(outcome.did_truncate());
    assert_eq!(written, outcome.kept.len() + outcome.archived.len());

    let bundles = captured.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0].session_id, "session-B");
    assert_eq!(bundles[0].total(), log.len());
    assert!(bundles[0].did_archive());
}

#[tokio::test]
async fn manager_default_budget_is_codex_5mib() {
    let mgr = RolloutManager::new(FakeArchiver::default());
    assert_eq!(mgr.max_bytes(), 5 * 1024 * 1024);
}
