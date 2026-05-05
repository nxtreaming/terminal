from __future__ import annotations

import tempfile
import time
import unittest
from pathlib import Path

from llm_browser.agent import SessionManager
from llm_browser.session.store import SessionStore


class SessionManagerTest(unittest.TestCase):
    def test_manager_runs_session_in_background(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            manager = SessionManager(store)

            session = manager.start("Open example.com", cwd=Path(tmp))

            deadline = time.time() + 3
            loaded = store.load(session.id)
            while time.time() < deadline and loaded is not None and loaded.status not in {"done", "failed"}:
                time.sleep(0.05)
                loaded = store.load(session.id)

            self.assertIsNotNone(loaded)
            self.assertEqual(loaded.status, "done")
            manager.reap()
            self.assertNotIn(session.id, manager.active())


if __name__ == "__main__":
    raise SystemExit(unittest.main())
