from __future__ import annotations

import tempfile
import time
import unittest
from pathlib import Path

from llm_browser.events.bus import EventBus
from llm_browser.events.event import Event
from llm_browser.events.store import EventStore
from llm_browser.session.store import SessionStore


class EventSpineTest(unittest.TestCase):
    def test_event_store_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = EventStore(Path(tmp))
            event = Event(type="tool.output", session_id="session-1", payload={"text": "hello"})

            store.append(event)

            events = store.read("session-1")
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0].type, "tool.output")
            self.assertEqual(events[0].payload, {"text": "hello"})

    def test_bus_subscriber_receives_published_events(self) -> None:
        bus = EventBus()
        event = Event(type="session.created", session_id="session-1")

        with bus.subscribe() as subscriber:
            bus.publish(event)
            received = subscriber.get(timeout=1)

        self.assertEqual(received.id, event.id)

    def test_session_store_creates_metadata_and_events(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bus = EventBus()
            store = SessionStore(Path(tmp), bus=bus)

            with bus.subscribe() as subscriber:
                session = store.create(cwd=Path(tmp))
                store.emit(session.id, "session.input", {"text": "task"})
                store.update_status(session.id, "idle")

                first = subscriber.get(timeout=1)
                second = subscriber.get(timeout=1)
                third = subscriber.get(timeout=1)

            loaded = store.load(session.id)
            self.assertIsNotNone(loaded)
            self.assertEqual(loaded.status, "idle")
            self.assertTrue(loaded.artifact_dir.exists())
            self.assertEqual(first.type, "session.created")
            self.assertEqual(second.type, "session.input")
            self.assertEqual(third.type, "session.status")
            self.assertEqual(third.payload, {"status": "idle"})
            self.assertEqual(len(store.events.read(session.id)), 3)

    def test_session_store_cancel_request_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))

            store.request_cancel(session.id, "test cancel")

            self.assertTrue(store.is_cancel_requested(session.id))
            self.assertEqual(store.cancel_request(session.id), {"reason": "test cancel"})
            self.assertEqual(store.load(session.id).status, "cancelled")  # type: ignore[union-attr]
            events = store.events.read(session.id)
            self.assertEqual(events[-2].type, "session.cancel_requested")
            self.assertEqual(events[-1].type, "session.status")
            self.assertEqual(events[-1].payload, {"status": "cancelled"})

            store.clear_cancel(session.id)
            self.assertFalse(store.is_cancel_requested(session.id))

    def test_session_store_reconciles_dead_runner_marker(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.update_status(session.id, "running")
            store.begin_run(session.id, pid=999999999)

            reconciled = store.reconcile_orphaned_sessions()

            self.assertEqual([item.id for item in reconciled], [session.id])
            self.assertIsNone(store.runner_info(session.id))
            loaded = store.load(session.id)
            self.assertIsNotNone(loaded)
            self.assertEqual(loaded.status, "failed")  # type: ignore[union-attr]
            events = store.events.read(session.id)
            self.assertEqual(events[-3].type, "session.reconciled")
            self.assertEqual(events[-2].type, "session.status")
            self.assertEqual(events[-1].type, "session.failed")
            self.assertEqual(events[-1].payload["error_type"], "StaleSession")

    def test_session_store_keeps_live_runner_marker(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.update_status(session.id, "running")
            store.begin_run(session.id)

            reconciled = store.reconcile_orphaned_sessions()

            self.assertEqual(reconciled, [])
            self.assertIsNotNone(store.runner_info(session.id))
            self.assertEqual(store.load(session.id).status, "running")  # type: ignore[union-attr]

    def test_session_store_can_reconcile_old_running_session_without_marker(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.update_status(session.id, "running")

            reconciled = store.reconcile_orphaned_sessions(stale_without_runner_ms=0)

            self.assertEqual([item.id for item in reconciled], [session.id])
            self.assertEqual(store.load(session.id).status, "failed")  # type: ignore[union-attr]

    def test_session_store_list_is_newest_first(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            older = store.create(cwd=Path(tmp))
            time.sleep(0.002)
            newer = store.create(cwd=Path(tmp))

            sessions = store.list()

            self.assertEqual([session.id for session in sessions], [newer.id, older.id])


if __name__ == "__main__":
    raise SystemExit(unittest.main())
