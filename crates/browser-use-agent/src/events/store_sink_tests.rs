//! Tests for the async [`StoreSink`] writer.
//!
//! These open a real, bundled-SQLite [`Store`] on a temp directory (no
//! network), emit events through the sink, then close the channel and await the
//! writer's [`JoinHandle`] before reading back to assert persistence. Reading
//! the store here is fine — the "write-only at runtime" rule is about the engine
//! hot path, not test assertions.
//!
//! `StoreSink::spawn` takes sole ownership of the `Store` (it unwraps the `Arc`
//! to move the `Store` onto its writer thread), so these tests hand the spawn
//! the only reference and then reopen a fresh `Store` on the same temp dir to
//! read the persisted rows back from the same SQLite file.

use std::sync::Arc;

use browser_use_store::Store;
use serde_json::json;

use super::store_sink::{ArtifactSpec, StoreSink};
use super::{names, EventSink, PendingEvent};

/// Open a fresh store on a temp dir and create a session. Returns the temp dir
/// (kept alive for the test) and the store-generated session id that
/// events/artifacts must reference (the FK requires the session row to exist).
fn setup_session() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = Store::open(dir.path()).expect("open store");
    let session = store
        .create_session(None, dir.path())
        .expect("create session");
    (dir, session.id)
}

#[tokio::test]
async fn emit_persists_events_in_order() {
    let (dir, session_id) = setup_session();
    let store = Arc::new(Store::open(dir.path()).expect("open store for writer"));

    let (sink, handle) = StoreSink::spawn(store);

    let events = vec![
        PendingEvent::new(&session_id, names::TASK_STARTED, json!({"n": 0})),
        PendingEvent::new(
            &session_id,
            names::MODEL_TURN_REQUEST,
            json!({"text": "hi"}),
        ),
        PendingEvent::new(&session_id, names::TOOL_STARTED, json!({"tool": "ls"})),
        PendingEvent::new(&session_id, names::TOOL_OUTPUT, json!({"out": "ok"})),
        PendingEvent::new(&session_id, names::TASK_COMPLETE, json!({"ok": true})),
    ];
    for ev in &events {
        sink.emit(ev.clone());
    }

    // Drop the sink to close the channel, then await the writer draining.
    drop(sink);
    handle.await.expect("writer task joins cleanly");

    // Reopen the store to read back (the writer owns the original handle).
    let reader = Store::open(dir.path()).expect("reopen store for reading");

    // `create_session` itself appends a "session.created" event (seq 1), so our
    // emitted events follow it. Filter to just the events we emitted.
    let persisted: Vec<_> = reader
        .events_for_session(&session_id)
        .expect("read events back")
        .into_iter()
        .filter(|e| e.event_type != "session.created")
        .collect();

    assert_eq!(
        persisted.len(),
        events.len(),
        "all emitted events should be persisted"
    );

    // Events come back ordered by seq ASC; verify type + payload order and that
    // sequence numbers are monotonically increasing.
    let mut last_seq = i64::MIN;
    for (got, expected) in persisted.iter().zip(events.iter()) {
        assert_eq!(got.session_id, expected.session_id);
        assert_eq!(got.event_type, expected.event_type);
        assert_eq!(got.payload, expected.payload);
        assert!(got.seq > last_seq, "sequence numbers must be increasing");
        last_seq = got.seq;
    }
}

#[tokio::test]
async fn emit_and_record_persists_event_and_seq_anchored_artifact() {
    let (dir, session_id) = setup_session();
    let store = Arc::new(Store::open(dir.path()).expect("open store for writer"));

    let (sink, handle) = StoreSink::spawn(store);

    // A plain event first, then an event-with-artifact, so the artifact's
    // anchored seq is unambiguous (it must be the artifact event's seq).
    sink.emit(PendingEvent::new(
        &session_id,
        names::TASK_STARTED,
        json!({"n": 0}),
    ));

    let art = ArtifactSpec {
        kind: "screenshot".to_string(),
        path: "shots/0001.png".to_string(),
        mime: "image/png".to_string(),
        metadata: json!({"w": 1280, "h": 720}),
    };
    sink.emit_and_record(
        PendingEvent::new(&session_id, names::ARTIFACT_CREATED, json!({"i": 0})),
        ArtifactSpec {
            kind: art.kind.clone(),
            path: art.path.clone(),
            mime: art.mime.clone(),
            metadata: art.metadata.clone(),
        },
    );

    drop(sink);
    handle.await.expect("writer task joins cleanly");

    let reader = Store::open(dir.path()).expect("reopen store for reading");

    // The artifact event itself was persisted.
    let events = reader
        .events_for_session(&session_id)
        .expect("read events back");
    let artifact_event = events
        .iter()
        .find(|e| e.event_type == names::ARTIFACT_CREATED)
        .expect("artifact event persisted");

    // The artifact was persisted and tied to the artifact event's seq.
    let artifacts = reader
        .artifacts_for_session(&session_id)
        .expect("read artifacts back");
    assert_eq!(
        artifacts.len(),
        1,
        "exactly one artifact should be recorded"
    );
    let row = &artifacts[0];
    assert_eq!(
        row.event_seq,
        Some(artifact_event.seq),
        "artifact must be anchored to the artifact event's seq"
    );
    assert_eq!(row.kind, art.kind);
    assert_eq!(row.path, art.path);
    assert_eq!(row.mime.as_deref(), Some(art.mime.as_str()));
}
