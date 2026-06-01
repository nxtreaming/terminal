//! WP-B4 async lifecycle tests against a REAL `browser_use_store::Store` on a tempfile
//! sqlite db (no network). Exercises create -> append -> resume, fork (LastN), rollback,
//! and the event-notify `wait_for_events_after_seq` (proving it is notification-driven, not
//! a poll).

use super::notifier::{spawn_notifier_bridge, wait_for_events_after_seq, StoreEventNotifier};
use super::sink::{SessionId, SharedStore, StoreEventSink, StoreEventSource};
use super::{EventSink, EventSource, ForkMode, Session};
use browser_use_store::Store;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Spin up a real store on a tempdir with the notifier bridge installed, returning the
/// shared store plus the sink / source / notifier handles. The tempdir is returned so the
/// caller keeps it alive for the duration of the test.
///
/// The store mints session ids itself and the `events` table has an FK on `sessions(id)`,
/// so the notifier mpsc must be installed at `open` time (there is no `set_notifier`).
fn setup() -> (
    tempfile::TempDir,
    SharedStore,
    Arc<StoreEventSink>,
    Arc<StoreEventSource>,
    StoreEventNotifier,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mpsc_tx, notifier) = spawn_notifier_bridge();
    let store = Store::open_with_notifier(dir.path(), mpsc_tx).expect("open store");
    let store: SharedStore = Arc::new(Mutex::new(store));
    let sink = Arc::new(StoreEventSink::new(Arc::clone(&store)));
    let source = Arc::new(StoreEventSource::new(Arc::clone(&store)));
    (dir, store, sink, source, notifier)
}

#[tokio::test]
async fn create_append_resume_roundtrips_history() {
    let (_dir, _store, sink, source, _notifier) = setup();

    // Create a brand-new root session and confirm the empty history install.
    let dyn_sink: Arc<dyn EventSink> = sink.clone();
    let session = Session::create(None, std::path::PathBuf::from("/tmp"), dyn_sink.clone())
        .await
        .expect("create");
    assert!(session.history().is_empty(), "fresh session has no history");
    let session_id = session.id().clone();

    // Append a couple of events through the (append-only) sink.
    sink.append_event(&session_id, "session.input", &json!({ "text": "hello" }))
        .await
        .expect("append user");
    sink.append_event(
        &session_id,
        "model.response.output_item",
        &json!({ "item": { "type": "message", "role": "assistant", "content": "hi there" } }),
    )
    .await
    .expect("append assistant");

    // Resume -> reconstructed history must equal the pure reducer over the same events.
    let resumed = Session::resume(session_id.clone(), source.clone(), dyn_sink)
        .await
        .expect("resume");

    let events = source
        .events_for_session(&session_id)
        .await
        .expect("events");
    let expected = super::reconstruct::provider_messages_from_events(&events);
    assert_eq!(resumed.history(), expected.as_slice());

    // Sanity: the reconstructed history contains the user "hello" and assistant "hi there".
    assert_eq!(resumed.history()[0]["role"], "user");
    let assistant = resumed
        .history()
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message");
    assert_eq!(assistant["content"], "hi there");

    // `create_session` emits a `session.created` event (seq 1); our two appends are 2 and 3.
    assert_eq!(resumed.last_seq(), 3);
}

#[tokio::test]
async fn fork_last_n_truncates_and_creates_child_row() {
    let (_dir, store, sink, source, _notifier) = setup();
    let dyn_sink: Arc<dyn EventSink> = sink.clone();

    let parent = Session::create(None, std::path::PathBuf::from("/tmp"), dyn_sink.clone())
        .await
        .expect("create parent");
    let parent_id = parent.id().clone();

    // Build a multi-turn parent history: two user turns, each with an assistant reply.
    for (user, reply) in [("first", "reply-1"), ("second", "reply-2")] {
        sink.append_event(&parent_id, "session.input", &json!({ "text": user }))
            .await
            .expect("append user");
        sink.append_event(
            &parent_id,
            "model.response.output_item",
            &json!({ "item": { "type": "message", "role": "assistant", "content": reply } }),
        )
        .await
        .expect("append assistant");
    }

    let full_events = source.events_for_session(&parent_id).await.expect("events");
    let full_history = super::reconstruct::provider_messages_from_events_for_fork(&full_events);
    assert!(
        full_history.len() >= 2,
        "parent should have a multi-message history: {full_history:?}"
    );

    // Fork keeping only the last fork turn (the trailing user+assistant turn).
    // LastN truncates by Codex fork-turn boundary, not by provider-message count.
    let child = parent
        .fork(source.clone(), dyn_sink.clone(), ForkMode::LastN(1))
        .await
        .expect("fork");

    assert_eq!(
        child.history().len(),
        2,
        "LastN(1) keeps the final user+assistant turn"
    );
    assert_eq!(
        &child.history()[..],
        &full_history[full_history.len() - 2..],
        "kept messages are the last provider messages from the final fork turn"
    );

    // The child is a real child row pointing at the parent (agent edge in the store).
    let child_id = child.id().clone();
    let children = {
        let store = store.lock().unwrap();
        store.list_child_agents(parent_id.as_str()).expect("agents")
    };
    assert!(
        children
            .iter()
            .any(|a| a.child_session_id == child_id.0 && a.parent_session_id == parent_id.0),
        "child row recorded with parent linkage: {children:?}"
    );

    // The carried history was seeded into the child's durable log, so a resume of the child
    // reconstructs a non-empty history.
    let resumed_child = Session::resume(child_id.clone(), source.clone(), dyn_sink)
        .await
        .expect("resume child");
    assert!(
        !resumed_child.history().is_empty(),
        "child seeded history reconstructs on resume"
    );
}

#[tokio::test]
async fn rollback_drops_last_user_turn() {
    let (_dir, _store, sink, source, _notifier) = setup();
    let dyn_sink: Arc<dyn EventSink> = sink.clone();

    let mut session = Session::create(None, std::path::PathBuf::from("/tmp"), dyn_sink.clone())
        .await
        .expect("create");
    let session_id = session.id().clone();

    // Turn 1 (kept).
    sink.append_event(&session_id, "session.input", &json!({ "text": "keep me" }))
        .await
        .unwrap();
    sink.append_event(
        &session_id,
        "model.response.output_item",
        &json!({ "item": { "type": "message", "role": "assistant", "content": "ok-1" } }),
    )
    .await
    .unwrap();
    // Turn 2 (to be rolled back).
    sink.append_event(
        &session_id,
        "session.followup",
        &json!({ "text": "drop me" }),
    )
    .await
    .unwrap();
    sink.append_event(
        &session_id,
        "model.response.output_item",
        &json!({ "item": { "type": "message", "role": "assistant", "content": "ok-2" } }),
    )
    .await
    .unwrap();

    // Resume to load the full 2-turn history first.
    session = Session::resume(session_id.clone(), source.clone(), dyn_sink)
        .await
        .expect("resume");
    assert_eq!(
        count_real_user_messages(session.history()),
        2,
        "two real user turns before rollback"
    );

    // Roll back one turn.
    let after_seq = session.rollback(1, source.clone()).await.expect("rollback");

    assert_eq!(
        count_real_user_messages(session.history()),
        1,
        "one real user turn survives after rollback(1)"
    );
    // The surviving user message is the first one; the dropped turn's text is gone.
    let text = serde_json::to_string(session.history()).unwrap();
    assert!(text.contains("keep me"), "first turn retained");
    assert!(!text.contains("drop me"), "second turn dropped");

    // The rollback recorded a durable `session.rollback` event at the rolled-back-to seq.
    let events = source.events_for_session(&session_id).await.unwrap();
    let rollback_ev = events
        .iter()
        .find(|e| e.event_type == "session.rollback")
        .expect("session.rollback persisted");
    assert_eq!(rollback_ev.payload["after_seq"], json!(after_seq));
}

#[tokio::test]
async fn wait_for_events_after_seq_wakes_on_append_not_poll() {
    let (_dir, _store, sink, source, notifier) = setup();
    let dyn_sink: Arc<dyn EventSink> = sink.clone();

    let session = Session::create(None, std::path::PathBuf::from("/tmp"), dyn_sink)
        .await
        .expect("create");
    let session_id = session.id().clone();

    // `create_session` already emitted `session.created` (seq 1). Start a waiter strictly
    // after the current tip with NO further events yet; it must park on the broadcast.
    let after = session.last_seq().max({
        let events = source.events_for_session(&session_id).await.unwrap();
        events.last().map(|e| e.seq).unwrap_or(0)
    });

    let waiter_source = source.clone();
    let waiter_notifier = notifier.clone();
    let waiter_id = session_id.clone();
    let waiter = tokio::spawn(async move {
        wait_for_events_after_seq(&waiter_notifier, waiter_source.as_ref(), &waiter_id, after).await
    });

    // Give the waiter a moment to subscribe and block, then append. The append fires a
    // store notification that the bridge relays to the broadcast, waking the waiter
    // promptly — without any 50ms poll. A generous timeout guards against a hang if the
    // notify path were broken.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let appended = sink
        .append_event(&session_id, "session.input", &json!({ "text": "ping" }))
        .await
        .expect("append");

    let events = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("waiter did not hang (event-notify works)")
        .expect("waiter task")
        .expect("waiter result");

    assert_eq!(events.len(), 1, "exactly the appended event is returned");
    assert_eq!(events[0].event_type, "session.input");
    assert_eq!(events[0].seq, appended.seq);
}

// --- small test-only helpers ----------------------------------------------------------

/// Count "real" user messages (codex `is_real_user_message`): role == user, no `name`, not
/// a `<turn_aborted>` marker. Inlined here to avoid depending on reducer-private predicates.
fn count_real_user_messages(history: &[Value]) -> usize {
    history
        .iter()
        .filter(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("name").is_none()
                && !message_text(m).contains("<turn_aborted>")
        })
        .count()
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

// Keep `SessionId` referenced for clarity even though it is constructed indirectly via
// `Session::id()`.
#[allow(dead_code)]
fn _assert_session_id_type(id: &SessionId) -> &str {
    id.as_str()
}
