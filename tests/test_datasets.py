from __future__ import annotations

import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from llm_browser.datasets import build_dataset_prompt, load_dataset, select_tasks, summarize_manifest
from llm_browser.cli import _dataset_manifest_exit_code, _resume_skip_task_ids, _run_dataset_task, _run_dataset_task_with_retries
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
        self.assertIn("load_skill('research')", prompt)
        self.assertIn("fetch_many_text", prompt)
        self.assertIn("directory/listing tasks", prompt)
        self.assertNotIn("Task-specific workflow", prompt)

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
                store.emit(session_id, "tool.started", {"name": "done", "arguments": {"result": "final answer"}})
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

    def test_dataset_task_requires_done_tool(self) -> None:
        class TextOnlyAgent:
            def __init__(self, *args, **kwargs) -> None:
                self.tools = Mock()

            def run_session(self, session_id: str, prompt: str):
                store.update_status(session_id, "done")
                store.emit(session_id, "session.done", {"result": "I should inspect the page next."})
                return store.load(session_id)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            with patch("llm_browser.cli.Agent", TextOnlyAgent):
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

        self.assertFalse(result["ok"])
        self.assertEqual(result["error_type"], "MissingDoneToolError")
        self.assertEqual(result["final_result"], "I should inspect the page next.")

    def test_dataset_task_rejects_interrupted_placeholder_result(self) -> None:
        class DoneAgent:
            def __init__(self, *args, **kwargs) -> None:
                self.tools = Mock()

            def run_session(self, session_id: str, prompt: str):
                store.update_status(session_id, "done")
                store.emit(session_id, "tool.started", {"name": "done", "arguments": {"result": "[Response interrupted]"}})
                store.emit(session_id, "session.done", {"result": "[Response interrupted]"})
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

        self.assertFalse(result["ok"])
        self.assertEqual(result["error_type"], "InterruptedResponseError")
        self.assertEqual(result["final_result"], "[Response interrupted]")

    def test_dataset_task_rejects_tool_error_final_result(self) -> None:
        class DoneAgent:
            def __init__(self, *args, **kwargs) -> None:
                self.tools = Mock()

            def run_session(self, session_id: str, prompt: str):
                store.update_status(session_id, "done")
                store.emit(
                    session_id,
                    "tool.started",
                    {"name": "done", "arguments": {"result": "[tool error: UnicodeDecodeError: binary]"}},
                )
                store.emit(session_id, "session.done", {"result": "[tool error: UnicodeDecodeError: binary]"})
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

        self.assertFalse(result["ok"])
        self.assertEqual(result["error_type"], "ToolErrorFinalResult")
        self.assertEqual(result["final_result"], "[tool error: UnicodeDecodeError: binary]")

    def test_dataset_task_retries_transient_codex_overload(self) -> None:
        calls = 0

        def fake_run_dataset_task(**kwargs):
            nonlocal calls
            calls += 1
            if calls == 1:
                return {
                    "task_id": kwargs["task_id"],
                    "ok": False,
                    "session": {"id": "failed-session"},
                    "error_type": "RuntimeError",
                    "error": (
                        "Codex stream error: {'type': 'error', 'error': {'type': "
                        "'service_unavailable_error', 'code': 'server_is_overloaded'}}"
                    ),
                }
            return {"task_id": kwargs["task_id"], "ok": True, "session": {"id": "ok-session"}}

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            with patch("llm_browser.cli._run_dataset_task", side_effect=fake_run_dataset_task), patch("llm_browser.cli.time.sleep"):
                result = _run_dataset_task_with_retries(
                    store=store,
                    task_id="1",
                    prompt="finish",
                    workspace=Path(tmp) / "work",
                    provider_name="codex",
                    model="gpt-5.5",
                    max_turns=1,
                    timeout_s=0,
                )

        self.assertTrue(result["ok"])
        self.assertEqual(calls, 2)
        self.assertEqual(result["attempt_number"], 2)
        self.assertEqual(result["retry_history"][0]["session_id"], "failed-session")

    def test_dataset_task_spills_large_final_result(self) -> None:
        final = "x" * 21000

        class DoneAgent:
            def __init__(self, *args, **kwargs) -> None:
                self.tools = Mock()

            def run_session(self, session_id: str, prompt: str):
                store.update_status(session_id, "done")
                store.emit(session_id, "tool.started", {"name": "done", "arguments": {"result": final}})
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

    def test_dataset_keyboard_interrupt_cancels_active_session(self) -> None:
        class InterruptingThread:
            def __init__(self, target, name=None, daemon=None) -> None:
                self.target = target

            def start(self) -> None:
                pass

            def join(self, timeout=None) -> None:
                raise KeyboardInterrupt

            def is_alive(self) -> bool:
                return False

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            with patch("llm_browser.cli.threading.Thread", InterruptingThread):
                with self.assertRaises(KeyboardInterrupt):
                    _run_dataset_task(
                        store=store,
                        task_id="1",
                        prompt="interrupt",
                        workspace=Path(tmp) / "work",
                        provider_name="fake",
                        model=None,
                        max_turns=1,
                        timeout_s=60,
                    )

            sessions = store.list()
            self.assertEqual(len(sessions), 1)
            self.assertEqual(sessions[0].status, "cancelled")
            cancel = store.cancel_request(sessions[0].id)
            self.assertEqual(cancel, {"reason": "dataset runner interrupted"})


if __name__ == "__main__":
    raise SystemExit(unittest.main())
