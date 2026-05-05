from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from llm_browser.agent import Agent
from llm_browser.session.store import SessionStore
from llm_browser.session.trace import build_self_eval_prompt, build_trace_bundle, write_trace_bundle


class TraceBundleTest(unittest.TestCase):
    def test_trace_bundle_and_self_eval_prompt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = Agent(store).run("trace me", cwd=Path(tmp))

            bundle = build_trace_bundle(store, session.id)
            path = write_trace_bundle(store, session.id)
            prompt = build_self_eval_prompt(store, session.id)

            self.assertEqual(bundle["session"]["id"], session.id)
            self.assertTrue(path.exists())
            self.assertIn("Evaluate this browser-use-terminal session trace", prompt)
            self.assertIn(str(path), prompt)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
