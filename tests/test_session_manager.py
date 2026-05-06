from __future__ import annotations

import tempfile
import time
import unittest
from pathlib import Path

from llm_browser.agent import SessionManager
from llm_browser.session.store import SessionStore


class SessionManagerTest(unittest.TestCase):
    def _wait_finished(self, store: SessionStore, session_id: str) -> None:
        deadline = time.time() + 3
        loaded = store.load(session_id)
        terminal_statuses = {"done", "failed", "cancelled"}
        while time.time() < deadline and loaded is not None and loaded.status not in terminal_statuses:
            time.sleep(0.05)
            loaded = store.load(session_id)
        self.assertIsNotNone(loaded)
        self.assertEqual(loaded.status, "done")

    def _wait_reaped(self, manager: SessionManager, session_id: str) -> None:
        deadline = time.time() + 3
        while time.time() < deadline:
            manager.reap()
            if session_id not in manager.active():
                return
            time.sleep(0.05)
        manager.reap()
        self.assertNotIn(session_id, manager.active())

    def _wait_resumed_done(self, store: SessionStore, session_id: str) -> None:
        deadline = time.time() + 3
        while time.time() < deadline:
            events = store.events.read(session_id)
            resumed_indices = [
                index
                for index, event in enumerate(events)
                if event.type == "session.input" and event.payload.get("resumed")
            ]
            resumed_index = resumed_indices[-1] if resumed_indices else None
            if resumed_index is not None and any(event.type == "session.done" for event in events[resumed_index + 1 :]):
                return
            time.sleep(0.05)
        self.fail("resumed session did not finish")

    def test_manager_runs_session_in_background(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            manager = SessionManager(store)

            session = manager.start("Open example.com", cwd=Path(tmp))

            self._wait_finished(store, session.id)
            self._wait_reaped(manager, session.id)

    def test_manager_resumes_session_in_background_without_creating_child(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            manager = SessionManager(store)

            session = manager.start("first", cwd=Path(tmp))
            self._wait_finished(store, session.id)
            first_tools = manager._tools[session.id]
            resumed = manager.resume(session.id, "second")
            self._wait_resumed_done(store, session.id)

            self.assertEqual(resumed.id, session.id)
            self.assertEqual([item.id for item in store.list()], [session.id])
            inputs = [event for event in store.events.read(session.id) if event.type == "session.input"]
            self.assertEqual([event.payload.get("text") for event in inputs], ["first", "second"])
            self.assertTrue(inputs[-1].payload.get("resumed"))
            self.assertIs(manager._tools[session.id], first_tools)
            manager.close(session.id, stop_browser=False)
            self.assertNotIn(session.id, manager._tools)
            self._wait_reaped(manager, session.id)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
