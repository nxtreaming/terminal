//! `context/workspace_context.rs` — workspace-context event helpers + event-record
//! rollback, ported from legacy `browser-use-core`.
//!
//! These are the durable-log helpers the tui/cli (and the run-entrypoint) call to
//! keep a session's workspace-context system events in sync with the cwd/workspace
//! as it changes, and to roll back filtered event records (transcript edit /
//! rollback view).
//!
//! ## What is ported here vs reused
//!
//! Legacy `append_workspace_context_event_with_options` (lib.rs:583) is a large
//! orchestration that *assembles* every workspace-context section (AGENTS.md load,
//! environment snapshot, collaboration mode, multi-agent hint, permissions) and
//! appends one `workspace.context` event **per `kind`** — but ONLY when that kind
//! is new or its content changed. The section-assembly half lives outside this leaf
//! (it depends on AGENTS.md loading / environment snapshotting that are not part of
//! the agent crate yet). The half that IS a self-contained leaf — and the part the
//! task asks to preserve — is the **append-or-skip change-detection / de-dup** over
//! the durable log:
//!   * [`has_context_kind`]        — legacy `has_context_kind` closure (lib.rs:590).
//!   * [`latest_context_content`]  — legacy `latest_context_content` closure
//!                                   (lib.rs:596): the LATEST `workspace.context`
//!                                   event of a kind, read newest-first.
//!   * [`append_workspace_context_event`] /
//!     [`append_workspace_context_event_with_options`] — append a single
//!     `workspace.context` event of a given kind, skipping the write when the kind's
//!     latest content already equals the new content (the de-dup), exactly as the
//!     per-kind branches in legacy do (e.g. environment kind, lib.rs:801-814).
//!   * [`append_user_shell_command_context_event`] — legacy
//!     `append_user_shell_command_context_event` (lib.rs:527), a fully self-contained
//!     leaf: it renders the `<user_shell_command>` block (with head/tail truncation
//!     at 40 000 chars) and appends an unconditional `user_shell_command`-kind event.
//!
//! For the in-prompt provider-message shapes (role/name/content of the *reconstructed*
//! workspace-context message) this REUSES [`super::inject`] + the
//! [`super::constants::WORKSPACE_CONTEXT_MESSAGE_NAME`] tag — this module only writes
//! the *event-log* side (`event_type = "workspace.context"`, the same literal the
//! reducer in [`crate::session::reconstruct`] consumes). The `kind` strings are the
//! existing `crate::session::reconstruct::WORKSPACE_CONTEXT_*_KIND` constants — reused,
//! not redefined.
//!
//! ## Rollback
//!
//! [`rollback_filtered_event_records`] is the pure reducer the transcript view uses to
//! collapse `session.rollback` events inline (dropping the last N user turns), ported
//! verbatim-in-behavior from legacy `rollback.rs:14`
//! (`rollback_filtered_event_records(events) -> rollback_filtered_events(events) ->
//! rollback_filtered_events_after(events, 0, &mut Vec::new())`). The `_after` reducer
//! already exists in [`crate::session::rollback`] (re-exported by `session::mod`), so
//! this is the thin public wrapper that legacy exposed under this name — it is NOT a
//! reimplementation.
//!
//! The store is the synchronous `browser_use_store::Store` held behind the
//! [`crate::session::SharedStore`] (`Arc<Mutex<Store>>`) the rest of the session infra
//! uses. Every store call is dispatched through `spawn_blocking` to keep the async
//! runtime unblocked, mirroring [`crate::session::sink`].

use std::time::Duration;

use browser_use_protocol::EventRecord;
use serde_json::Value;

use crate::session::reconstruct::{
    WORKSPACE_CONTEXT_ENVIRONMENT_KIND, WORKSPACE_CONTEXT_USER_SHELL_KIND,
};
use crate::session::SharedStore;

/// Durable event type for the workspace-context system events. Exact literal from
/// legacy `browser-use-core` (`store.append_event(.., "workspace.context", ..)`) and
/// the value the reducer in [`crate::session::reconstruct`] matches on.
pub const WORKSPACE_CONTEXT_EVENT_TYPE: &str = "workspace.context";

/// True iff the log already contains a `workspace.context` event of `kind`.
///
/// Parity: legacy `has_context_kind` closure (lib.rs:590), `events.iter().any(..)`
/// over `event_type == "workspace.context" && payload.kind == kind`.
pub fn has_context_kind(events: &[EventRecord], kind: &str) -> bool {
    events.iter().any(|event| {
        event.event_type == WORKSPACE_CONTEXT_EVENT_TYPE
            && event.payload.get("kind").and_then(Value::as_str) == Some(kind)
    })
}

/// The `content` of the LATEST `workspace.context` event of `kind`, if any.
///
/// Parity: legacy `latest_context_content` closure (lib.rs:596): walk the log
/// newest-first (`events.iter().rev().find_map(..)`) and return the first matching
/// kind's `payload.content` as `&str`.
pub fn latest_context_content<'a>(events: &'a [EventRecord], kind: &str) -> Option<&'a str> {
    events.iter().rev().find_map(|event| {
        (event.event_type == WORKSPACE_CONTEXT_EVENT_TYPE
            && event.payload.get("kind").and_then(Value::as_str) == Some(kind))
        .then(|| event.payload.get("content").and_then(Value::as_str))
        .flatten()
    })
}

/// Append a `workspace.context` event of `kind` carrying `content`, *unless* the
/// log's latest event of that kind already carries identical content (de-dup).
///
/// Returns `true` iff an event was appended.
///
/// This is the per-kind append-or-skip change-detection legacy applies (e.g. the
/// environment-kind branch, lib.rs:801-814: append only when
/// `latest_context_content(kind) != Some(content)`). The non-forcing wrapper.
pub async fn append_workspace_context_event(
    store: SharedStore,
    session_id: &str,
    kind: &str,
    content: String,
) -> anyhow::Result<bool> {
    append_workspace_context_event_with_options(store, session_id, kind, content, false).await
}

/// Append a `workspace.context` event of `kind`, optionally forcing the write.
///
/// When `!force`, the write is skipped (returning `false`) if the latest
/// `workspace.context` event of `kind` already carries identical `content` — the
/// exact change-detection / de-dup legacy performs per kind. When `force` is set the
/// event is always appended.
///
/// Returns `true` iff an event was appended.
pub async fn append_workspace_context_event_with_options(
    store: SharedStore,
    session_id: &str,
    kind: &str,
    content: String,
    force: bool,
) -> anyhow::Result<bool> {
    let session_id = session_id.to_string();
    let kind = kind.to_string();

    spawn_blocking_store(move || {
        let store = store.lock().expect("store mutex poisoned");

        let events = store.events_for_session(&session_id)?;
        if !force && latest_context_content(&events, &kind) == Some(content.as_str()) {
            return Ok(false);
        }

        let payload = serde_json::json!({
            "kind": kind,
            "content": content,
        });
        store.append_event(&session_id, WORKSPACE_CONTEXT_EVENT_TYPE, payload)?;
        Ok(true)
    })
    .await
}

/// Append a `user_shell_command`-kind `workspace.context` event recording a shell
/// command the user ran (command, exit code, duration, output).
///
/// Parity: legacy `append_user_shell_command_context_event` (lib.rs:527),
/// byte-identical: the `<user_shell_command>` content block (output passed through
/// [`user_shell_context_output`]), and the payload
/// `{kind, content, command, exit_code, duration_ms}`. This event is appended
/// unconditionally (legacy does not de-dup user-shell events).
pub async fn append_user_shell_command_context_event(
    store: SharedStore,
    session_id: &str,
    command: &str,
    exit_code: i32,
    duration: Duration,
    output: &str,
) -> anyhow::Result<()> {
    let session_id = session_id.to_string();
    let command = command.to_string();
    let output = user_shell_context_output(output);
    let content = format!(
        "<user_shell_command>\n<command>\n{}\n</command>\n<result>\nExit code: {}\nDuration: {:.4} seconds\nOutput:\n{}\n</result>\n</user_shell_command>",
        command,
        exit_code,
        duration.as_secs_f64(),
        output,
    );

    spawn_blocking_store(move || {
        let store = store.lock().expect("store mutex poisoned");
        store.append_event(
            &session_id,
            WORKSPACE_CONTEXT_EVENT_TYPE,
            serde_json::json!({
                "kind": WORKSPACE_CONTEXT_USER_SHELL_KIND,
                "content": content,
                "command": command,
                "exit_code": exit_code,
                "duration_ms": duration.as_millis() as u64,
            }),
        )?;
        Ok(())
    })
    .await
}

/// Head/tail-truncate user shell output to `MAX_USER_SHELL_CONTEXT_CHARS` characters.
///
/// Parity: legacy `user_shell_context_output` (lib.rs:557), byte-identical: keep the
/// first half and last half of the budget (char-wise), with an
/// `[... omitted N characters from user shell output ...]` marker between them.
fn user_shell_context_output(output: &str) -> String {
    const MAX_USER_SHELL_CONTEXT_CHARS: usize = 40_000;
    let char_count = output.chars().count();
    if char_count <= MAX_USER_SHELL_CONTEXT_CHARS {
        return output.to_string();
    }
    let head_budget = MAX_USER_SHELL_CONTEXT_CHARS / 2;
    let tail_budget = MAX_USER_SHELL_CONTEXT_CHARS.saturating_sub(head_budget);
    let head = output.chars().take(head_budget).collect::<String>();
    let tail = output
        .chars()
        .rev()
        .take(tail_budget)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!(
        "{head}\n[... omitted {} characters from user shell output ...]\n{tail}",
        char_count.saturating_sub(head_budget + tail_budget)
    )
}

/// Roll back filtered event records: the durable log with every `session.rollback`
/// event applied inline (each drops the prior N real user turns), returning the
/// surviving event records in original order.
///
/// Used by the transcript view to render the post-rollback event history.
///
/// Parity: legacy `rollback_filtered_event_records` (rollback.rs:14), which is
/// `rollback_filtered_events(events)` ->
/// `rollback_filtered_events_after(events, 0, &mut Vec::new())`. The `_after` reducer
/// already lives in [`crate::session::rollback`] (re-exported by `session`); this is
/// the thin public wrapper legacy exposed under this name — NOT a reimplementation.
pub fn rollback_filtered_event_records(events: &[EventRecord]) -> Vec<&EventRecord> {
    let mut messages: Vec<Value> = Vec::new();
    crate::session::rollback_filtered_events_after(events, 0, &mut messages)
}

/// Convenience: append an `environment_context`-kind workspace-context event with the
/// per-kind de-dup. The environment kind is the one legacy re-emits across turns as
/// the cwd/workspace changes, so it is the canonical "refresh on change" caller; the
/// generic [`append_workspace_context_event`] handles any kind.
pub async fn append_environment_context_event(
    store: SharedStore,
    session_id: &str,
    content: String,
) -> anyhow::Result<bool> {
    append_workspace_context_event(
        store,
        session_id,
        WORKSPACE_CONTEXT_ENVIRONMENT_KIND,
        content,
    )
    .await
}

/// Run a synchronous store closure on the blocking pool and flatten the join error.
///
/// Mirrors [`crate::session::sink`]'s `spawn_blocking_store`: the store is synchronous
/// (rusqlite), so its calls run on the blocking pool and the `JoinError` is flattened
/// into the returned `anyhow::Result`.
async fn spawn_blocking_store<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|join_err| anyhow::anyhow!("store task panicked: {join_err}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_use_store::Store;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// A tempdir-backed `SharedStore` plus a fresh session id. The `TempDir` is
    /// returned so the caller keeps it alive (dropping it deletes the on-disk sqlite
    /// db). Pattern copied from `infra/persistence.rs` tests
    /// (`Store::open(dir.path())` + `create_session`).
    fn shared_store_with_session() -> (TempDir, SharedStore, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");
        let session_id = store
            .create_session(None, std::path::Path::new("/tmp"))
            .expect("create session")
            .id;
        (dir, Arc::new(Mutex::new(store)), session_id)
    }

    fn events(store: &SharedStore, session_id: &str) -> Vec<EventRecord> {
        store
            .lock()
            .unwrap()
            .events_for_session(session_id)
            .unwrap()
    }

    fn workspace_context_events(store: &SharedStore, session_id: &str) -> Vec<EventRecord> {
        events(store, session_id)
            .into_iter()
            .filter(|e| e.event_type == WORKSPACE_CONTEXT_EVENT_TYPE)
            .collect()
    }

    #[tokio::test]
    async fn append_when_new_kind_writes_event() {
        let (_dir, store, session_id) = shared_store_with_session();

        // No prior environment-kind event -> always writes.
        let appended = append_environment_context_event(
            Arc::clone(&store),
            &session_id,
            "cwd: /a".to_string(),
        )
        .await
        .unwrap();
        assert!(appended);

        let ws = workspace_context_events(&store, &session_id);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].payload["kind"], WORKSPACE_CONTEXT_ENVIRONMENT_KIND);
        assert_eq!(ws[0].payload["content"], "cwd: /a");
    }

    #[tokio::test]
    async fn append_when_changed_writes_second_event() {
        let (_dir, store, session_id) = shared_store_with_session();

        append_environment_context_event(Arc::clone(&store), &session_id, "cwd: /a".to_string())
            .await
            .unwrap();
        // Changed content -> a second event is appended.
        let appended = append_environment_context_event(
            Arc::clone(&store),
            &session_id,
            "cwd: /b".to_string(),
        )
        .await
        .unwrap();
        assert!(appended);

        let ws = workspace_context_events(&store, &session_id);
        let contents: Vec<&str> = ws
            .iter()
            .map(|e| e.payload["content"].as_str().unwrap())
            .collect();
        assert_eq!(contents, vec!["cwd: /a", "cwd: /b"]);
    }

    #[tokio::test]
    async fn no_append_when_content_unchanged() {
        let (_dir, store, session_id) = shared_store_with_session();

        let first = append_environment_context_event(
            Arc::clone(&store),
            &session_id,
            "cwd: /a".to_string(),
        )
        .await
        .unwrap();
        assert!(first);
        // Identical content -> de-dup: no new event, returns false.
        let second = append_environment_context_event(
            Arc::clone(&store),
            &session_id,
            "cwd: /a".to_string(),
        )
        .await
        .unwrap();
        assert!(!second);

        assert_eq!(workspace_context_events(&store, &session_id).len(), 1);
    }

    #[tokio::test]
    async fn force_appends_even_when_unchanged() {
        let (_dir, store, session_id) = shared_store_with_session();

        append_workspace_context_event_with_options(
            Arc::clone(&store),
            &session_id,
            WORKSPACE_CONTEXT_ENVIRONMENT_KIND,
            "cwd: /a".to_string(),
            false,
        )
        .await
        .unwrap();
        // force = true bypasses the change-detection de-dup.
        let appended = append_workspace_context_event_with_options(
            Arc::clone(&store),
            &session_id,
            WORKSPACE_CONTEXT_ENVIRONMENT_KIND,
            "cwd: /a".to_string(),
            true,
        )
        .await
        .unwrap();
        assert!(appended);

        assert_eq!(workspace_context_events(&store, &session_id).len(), 2);
    }

    #[tokio::test]
    async fn dedup_uses_latest_event_of_kind() {
        let (_dir, store, session_id) = shared_store_with_session();

        // env(/a), then env(/b), then env(/a) again. The de-dup compares against the
        // LATEST env event (/b), so env(/a) is a change and IS appended.
        append_environment_context_event(Arc::clone(&store), &session_id, "cwd: /a".to_string())
            .await
            .unwrap();
        append_environment_context_event(Arc::clone(&store), &session_id, "cwd: /b".to_string())
            .await
            .unwrap();
        let appended = append_environment_context_event(
            Arc::clone(&store),
            &session_id,
            "cwd: /a".to_string(),
        )
        .await
        .unwrap();
        assert!(appended);

        assert_eq!(workspace_context_events(&store, &session_id).len(), 3);
    }

    #[tokio::test]
    async fn dedup_is_per_kind() {
        let (_dir, store, session_id) = shared_store_with_session();

        // Same content but different kinds -> both append (de-dup is per-kind).
        append_workspace_context_event(
            Arc::clone(&store),
            &session_id,
            WORKSPACE_CONTEXT_ENVIRONMENT_KIND,
            "same".to_string(),
        )
        .await
        .unwrap();
        let appended = append_workspace_context_event(
            Arc::clone(&store),
            &session_id,
            "agents_md",
            "same".to_string(),
        )
        .await
        .unwrap();
        assert!(appended);

        assert_eq!(workspace_context_events(&store, &session_id).len(), 2);
    }

    #[test]
    fn has_context_kind_and_latest_content() {
        fn ws_event(seq: i64, kind: &str, content: &str) -> EventRecord {
            EventRecord {
                seq,
                id: format!("e{seq}"),
                session_id: "s".to_string(),
                ts_ms: seq,
                event_type: WORKSPACE_CONTEXT_EVENT_TYPE.to_string(),
                payload: serde_json::json!({ "kind": kind, "content": content }),
            }
        }
        let log = vec![
            ws_event(1, WORKSPACE_CONTEXT_ENVIRONMENT_KIND, "old"),
            ws_event(2, "agents_md", "agents"),
            ws_event(3, WORKSPACE_CONTEXT_ENVIRONMENT_KIND, "new"),
        ];

        assert!(has_context_kind(&log, WORKSPACE_CONTEXT_ENVIRONMENT_KIND));
        assert!(has_context_kind(&log, "agents_md"));
        assert!(!has_context_kind(&log, "permissions"));

        // Latest env content is the newest one.
        assert_eq!(
            latest_context_content(&log, WORKSPACE_CONTEXT_ENVIRONMENT_KIND),
            Some("new")
        );
        assert_eq!(latest_context_content(&log, "agents_md"), Some("agents"));
        assert_eq!(latest_context_content(&log, "permissions"), None);
    }

    #[tokio::test]
    async fn user_shell_command_event_renders_block_and_appends() {
        let (_dir, store, session_id) = shared_store_with_session();

        append_user_shell_command_context_event(
            Arc::clone(&store),
            &session_id,
            "ls -la",
            0,
            Duration::from_millis(1500),
            "file_a\nfile_b",
        )
        .await
        .unwrap();

        let ws = workspace_context_events(&store, &session_id);
        assert_eq!(ws.len(), 1);
        let ev = &ws[0];
        assert_eq!(ev.payload["kind"], WORKSPACE_CONTEXT_USER_SHELL_KIND);
        assert_eq!(ev.payload["command"], "ls -la");
        assert_eq!(ev.payload["exit_code"], 0);
        assert_eq!(ev.payload["duration_ms"], 1500);
        let content = ev.payload["content"].as_str().unwrap();
        assert!(content.starts_with("<user_shell_command>\n<command>\nls -la\n</command>"));
        assert!(content.contains("Exit code: 0"));
        assert!(content.contains("Duration: 1.5000 seconds"));
        assert!(content.contains("Output:\nfile_a\nfile_b"));
        assert!(content.ends_with("</user_shell_command>"));
    }

    #[test]
    fn user_shell_context_output_truncates_oversized() {
        let small = "abc";
        assert_eq!(user_shell_context_output(small), "abc");

        let big = "x".repeat(50_000);
        let out = user_shell_context_output(&big);
        assert!(out.contains("[... omitted 10000 characters from user shell output ...]"));
        // Head + tail keep exactly the 40k budget plus the marker line.
        assert!(out.starts_with(&"x".repeat(20_000)));
        assert!(out.ends_with(&"x".repeat(20_000)));
    }

    // ---- rollback_filtered_event_records (pure reducer wrapper) ----

    fn event(seq: i64, ty: &str, payload: Value) -> EventRecord {
        EventRecord {
            seq,
            id: format!("e{seq}"),
            session_id: "s".to_string(),
            ts_ms: seq,
            event_type: ty.to_string(),
            payload,
        }
    }

    #[test]
    fn rollback_without_rollback_event_returns_all() {
        let log = vec![
            event(1, "session.input", serde_json::json!({ "text": "a" })),
            event(2, "agent.message", serde_json::json!({ "content": "hi" })),
        ];
        let kept = rollback_filtered_event_records(&log);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].seq, 1);
        assert_eq!(kept[1].seq, 2);
    }

    #[test]
    fn rollback_drops_last_user_turn() {
        // Two real user turns (`session.input`) with a neutral, non-turn-boundary
        // tool event in between, then a `session.rollback(num_turns:1)`.
        //
        // The reducer (`rollback_filtered_events_after` -> `rollback_last_n_user_turns`)
        // drops the LAST real user turn: it rposition-finds the last `session.input`
        // (seq 4) and truncates the replay there, so seq 4 (and anything after it) is
        // dropped. The rollback event itself is consumed (not replayed). The first
        // user turn and its trailing tool event survive.
        let log = vec![
            event(1, "session.input", serde_json::json!({ "text": "first" })),
            event(2, "tool.output", serde_json::json!({ "text": "ran" })),
            event(3, "tool.output", serde_json::json!({ "text": "ran2" })),
            event(4, "session.input", serde_json::json!({ "text": "second" })),
            event(5, "session.rollback", serde_json::json!({ "num_turns": 1 })),
        ];
        let kept = rollback_filtered_event_records(&log);
        let kept_seqs: Vec<i64> = kept.iter().map(|e| e.seq).collect();
        // The second user turn (seq 4) is dropped; the first turn (seq 1) plus its
        // trailing non-user events survive; the rollback event is not replayed.
        assert_eq!(kept_seqs, vec![1, 2, 3]);
    }
}
