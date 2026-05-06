from __future__ import annotations

import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from llm_browser.datasets import build_dataset_prompt, load_dataset, select_tasks, summarize_manifest
from llm_browser.cli import _dataset_manifest_exit_code, _resume_skip_task_ids, _run_dataset_task
from llm_browser.session.store import SessionStore


class DatasetTest(unittest.TestCase):
    def test_load_and_select_real_v8(self) -> None:
        tasks = load_dataset("real_v8", cwd=Path.cwd())

        self.assertGreaterEqual(len(tasks), 1)
        selected = select_tasks(tasks, count=2, seed=123)
        self.assertEqual(len(selected), 2)
        self.assertNotEqual(selected[0].task_id, selected[1].task_id)

    def test_prompt_wraps_task_without_hiding_original_text(self) -> None:
        task = select_tasks(load_dataset("real_v14_short", cwd=Path.cwd()), task_ids=["2"])[0]
        prompt = build_dataset_prompt(task, headless=True)

        self.assertIn("Dataset:", prompt)
        self.assertIn("Task ID: 2", prompt)
        self.assertIn(task.text[:80], prompt)
        self.assertIn("Attach screenshots", prompt)
        self.assertIn("output_path('/home/user/outputs/name.ext')", prompt)
        self.assertIn("fetch_many_text", prompt)

    def test_summarize_manifest_uses_latest_attempt(self) -> None:
        manifest = {
            "run_id": "r1",
            "dataset": "real_v8",
            "provider": "fake",
            "model": "gpt-test",
            "selection": [{"task_id": "1"}, {"task_id": "2"}],
            "sessions": [
                {"task_id": "1", "ok": False},
                {"task_id": "1", "ok": True},
            ],
        }

        summary = summarize_manifest(manifest)

        self.assertEqual(summary["passed_task_ids"], ["1"])
        self.assertEqual(summary["pending_task_ids"], ["2"])
        self.assertEqual(summary["attempts_by_task"], {"1": 2})

    def test_dataset_exit_code_uses_latest_attempt(self) -> None:
        manifest = {
            "selection": [{"task_id": "1"}],
            "sessions": [
                {"task_id": "1", "ok": False},
                {"task_id": "1", "ok": True},
            ],
        }

        self.assertEqual(_dataset_manifest_exit_code(manifest), 0)

    def test_resume_skip_task_ids_can_include_failed_attempts(self) -> None:
        manifest = {
            "selection": [{"task_id": "1"}, {"task_id": "2"}, {"task_id": "3"}],
            "sessions": [
                {"task_id": "1", "ok": True},
                {"task_id": "2", "ok": False},
            ],
        }

        self.assertEqual(_resume_skip_task_ids(manifest), {"1"})
        self.assertEqual(_resume_skip_task_ids(manifest, skip_failed=True), {"1", "2"})

    def test_dataset_task_records_final_result(self) -> None:
        class DoneAgent:
            def __init__(self, *args, **kwargs) -> None:
                self.tools = Mock()

            def run_session(self, session_id: str, prompt: str):
                store.update_status(session_id, "done")
                store.emit(session_id, "session.done", {"result": "final answer"})
                return store.load(session_id)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            with patch("llm_browser.cli.Agent", DoneAgent):
                result = _run_dataset_task(
                    store=store,
                    task_id="1",
                    prompt="finish",
                    workspace=Path(tmp) / "work",
                    provider_name="fake",
                    model=None,
                    max_turns=1,
                    timeout_s=0,
                )

        self.assertTrue(result["ok"])
        self.assertEqual(result["final_result"], "final answer")
        self.assertEqual(result["final_result_chars"], len("final answer"))

    def test_dataset_task_spills_large_final_result(self) -> None:
        final = "x" * 21000

        class DoneAgent:
            def __init__(self, *args, **kwargs) -> None:
                self.tools = Mock()

            def run_session(self, session_id: str, prompt: str):
                store.update_status(session_id, "done")
                store.emit(session_id, "session.done", {"result": final})
                return store.load(session_id)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            with patch("llm_browser.cli.Agent", DoneAgent):
                result = _run_dataset_task(
                    store=store,
                    task_id="1",
                    prompt="finish",
                    workspace=Path(tmp) / "work",
                    provider_name="fake",
                    model=None,
                    max_turns=1,
                    timeout_s=0,
                )

            self.assertTrue(result["ok"])
            self.assertNotIn("final_result", result)
            self.assertEqual(result["final_result_chars"], len(final))
            self.assertTrue(Path(result["final_result_path"]).exists())
            self.assertEqual(Path(result["final_result_path"]).read_text(encoding="utf-8"), final)

    def test_dataset_timeout_closes_agent_tools(self) -> None:
        unblocked = threading.Event()
        close_session = Mock(side_effect=lambda session_id: unblocked.set())

        class HangingAgent:
            def __init__(self, *args, **kwargs) -> None:
                self.tools = Mock(close_session=close_session)

            def run_session(self, session_id: str, prompt: str):
                unblocked.wait(5)
                return store.update_status(session_id, "cancelled")

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            with patch("llm_browser.cli.Agent", HangingAgent):
                result = _run_dataset_task(
                    store=store,
                    task_id="1",
                    prompt="hang",
                    workspace=Path(tmp) / "work",
                    provider_name="fake",
                    model=None,
                    max_turns=1,
                    timeout_s=0.01,
                )

        self.assertFalse(result["ok"])
        self.assertEqual(result["error_type"], "TimeoutError")
        close_session.assert_called_once()

    def test_dataset_timeout_requires_restart_when_worker_keeps_running(self) -> None:
        release = threading.Event()
        close_session = Mock()

        class StuckAgent:
            def __init__(self, *args, **kwargs) -> None:
                self.tools = Mock(close_session=close_session)

            def run_session(self, session_id: str, prompt: str):
                release.wait(5)
                return store.load(session_id)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            try:
                with patch("llm_browser.cli.Agent", StuckAgent), patch("llm_browser.cli.DATASET_TIMEOUT_DRAIN_S", 0.01):
                    result = _run_dataset_task(
                        store=store,
                        task_id="1",
                        prompt="hang",
                        workspace=Path(tmp) / "work",
                        provider_name="fake",
                        model=None,
                        max_turns=1,
                        timeout_s=0.01,
                    )
            finally:
                release.set()

        self.assertFalse(result["ok"])
        self.assertEqual(result["error_type"], "TimeoutError")
        self.assertTrue(result["fatal_runner_restart_required"])
        self.assertIn("runner must restart", result["error"])
        close_session.assert_called_once()


if __name__ == "__main__":
    raise SystemExit(unittest.main())
