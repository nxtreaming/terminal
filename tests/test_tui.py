from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from llm_browser.events import Event
from llm_browser.session.store import SessionStore
from llm_browser.tui.app import (
    BrowserUseTerminalApp,
    CommandPalette,
    SessionPalette,
    _artifact_kind,
    _current_tool,
    _dataset_run_id_from_path,
    _dataset_run_label,
    _dataset_task_id_from_path,
    _final_line,
    _format_events_for_log,
    _latest_image_line,
    _progress_bar,
    _short_task_list,
    _summarize_task_text,
)
from llm_browser.tui import format_event


class TuiTest(unittest.TestCase):
    def test_format_image_event(self) -> None:
        event = Event(
            type="tool.image",
            session_id="s1",
            payload={"image": {"label": "loaded", "path": "/tmp/loaded.png"}},
        )

        self.assertEqual(format_event(event), "[s1] image: loaded -> /tmp/loaded.png")

    def test_format_tool_finished_truncates_output(self) -> None:
        event = Event(
            type="tool.finished",
            session_id="s1",
            payload={"name": "shell", "output": {"text": "a" * 200}},
        )

        formatted = format_event(event)
        self.assertIn("[s1] tool done: shell", formatted)
        self.assertLess(len(formatted), 210)

    def test_format_done_truncates_large_final_result(self) -> None:
        event = Event(
            type="session.done",
            session_id="s1",
            payload={"result": "x" * 1000},
        )

        formatted = format_event(event)

        self.assertIn("[s1] done:", formatted)
        self.assertLess(len(formatted), 280)

    def test_format_tool_output_truncates_stream_chunk(self) -> None:
        event = Event(
            type="tool.output",
            session_id="s1",
            payload={"name": "shell", "stream": "stdout", "text": "b" * 200},
        )

        formatted = format_event(event)
        self.assertIn("[s1] tool output: shell stdout", formatted)
        self.assertLess(len(formatted), 230)

    def test_format_events_for_log_coalesces_model_deltas(self) -> None:
        events = [
            Event(type="model.delta", session_id="s1", payload={"text": "Hel"}),
            Event(type="model.delta", session_id="s1", payload={"text": "lo"}),
            Event(type="tool.started", session_id="s1", payload={"name": "python", "tool_call_id": "c1"}),
        ]

        lines = _format_events_for_log(events)

        self.assertEqual(lines[0], ("[s1] model: Hello", "model.delta"))
        self.assertIn("tool start: python", lines[1][0])

    def test_summarize_task_text_extracts_dataset_task(self) -> None:
        text = (
            "You are running a browser-use-terminal dataset task.\n"
            "Dataset: real_v8\n\n"
            "Task:\n"
            "Visit a site and save the result.\n\n"
            "Runtime budget: this task has a hard timeout."
        )

        self.assertEqual(_summarize_task_text(text), "Visit a site and save the result.")

    def test_current_tool_tracks_latest_unfinished_call(self) -> None:
        events = [
            Event(type="tool.started", session_id="s1", payload={"name": "python", "tool_call_id": "c1"}),
            Event(type="tool.finished", session_id="s1", payload={"name": "python", "tool_call_id": "c1"}),
            Event(type="tool.started", session_id="s1", payload={"name": "python", "tool_call_id": "c2"}),
        ]

        self.assertEqual(_current_tool(events), "python c2")

    def test_current_tool_falls_back_to_last_finished_call(self) -> None:
        events = [
            Event(type="tool.started", session_id="s1", payload={"name": "python", "tool_call_id": "c1"}),
            Event(type="tool.finished", session_id="s1", payload={"name": "python", "tool_call_id": "c1"}),
        ]

        self.assertEqual(_current_tool(events), "python done")

    def test_final_line_prefers_terminal_session_event(self) -> None:
        events = [
            Event(type="tool.finished", session_id="s1", payload={"name": "python", "output": {"text": "intermediate"}}),
            Event(type="session.done", session_id="s1", payload={"result": "complete"}),
        ]

        self.assertEqual(_final_line(events), "complete")

    def test_short_task_list_limits_long_runs(self) -> None:
        self.assertEqual(_short_task_list([]), "-")
        self.assertEqual(_short_task_list([str(index) for index in range(14)], limit=3), "0, 1, 2 +11")

    def test_dataset_run_id_from_workspace_path(self) -> None:
        path = Path("/tmp/state/dataset-runs/real-v8-gpt55-full/task-1-workspace")

        self.assertEqual(_dataset_run_id_from_path(path), "real-v8-gpt55-full")

    def test_dataset_task_id_and_run_label_from_workspace_path(self) -> None:
        path = Path("/tmp/state/dataset-runs/real-v8-gpt55-full/task-100-workspace")

        self.assertEqual(_dataset_task_id_from_path(path), "100")
        self.assertEqual(_dataset_run_label(path), "v8-gpt55-full:100")

    def test_progress_bar_renders_filled_and_empty_cells(self) -> None:
        rendered = _progress_bar(3, 6, width=6)

        self.assertIn("███", rendered)
        self.assertIn("░░░", rendered)

    def test_latest_image_line_uses_most_recent_image(self) -> None:
        events = [
            Event(type="tool.image", session_id="s1", payload={"image": {"label": "first", "path": "/tmp/1.png"}}),
            Event(type="tool.image", session_id="s1", payload={"image": {"label": "final", "path": "/tmp/final.png"}}),
        ]

        self.assertEqual(_latest_image_line(events), "final -> final.png")

    def test_artifact_kind_prioritizes_trace_and_download_dirs(self) -> None:
        self.assertEqual(_artifact_kind(Path("/tmp/session/browser/traces/001_trace.json")), "trace")
        self.assertEqual(_artifact_kind(Path("/tmp/session/browser/downloads/report.csv")), "download")


class TuiInteractionTest(unittest.IsolatedAsyncioTestCase):
    async def test_tui_starts_without_selecting_existing_session(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            store.create()
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.pause()

            self.assertIsNone(app.selected_session_id)

    async def test_command_palette_supports_arrow_and_vim_navigation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.press("ctrl+p")
                await pilot.pause()
                self.assertIsInstance(app.screen, CommandPalette)
                table = app.screen.query_one("#palette-table")

                self.assertEqual(table.cursor_row, 0)
                await pilot.press("down")
                await pilot.pause()
                self.assertEqual(table.cursor_row, 1)
                await pilot.press("j")
                await pilot.pause()
                self.assertEqual(table.cursor_row, 2)
                await pilot.press("k")
                await pilot.pause()
                self.assertEqual(table.cursor_row, 1)
                await pilot.press("G")
                await pilot.pause()
                self.assertEqual(table.cursor_row, table.row_count - 1)
                await pilot.press("g")
                await pilot.pause()
                self.assertEqual(table.cursor_row, 0)
                await pilot.press("escape")
                await pilot.pause()
                self.assertNotIsInstance(app.screen, CommandPalette)

    async def test_session_palette_supports_vim_navigation_and_enter_selection(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            store.create()
            store.create()
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.press("tab")
                await pilot.pause()
                self.assertIsInstance(app.screen, SessionPalette)
                screen = app.screen
                table = screen.query_one("#sessions-table")
                visible_ids = list(screen._visible_session_ids)

                self.assertGreaterEqual(len(visible_ids), 2)
                await pilot.press("j")
                await pilot.pause()
                expected = visible_ids[table.cursor_row]
                await pilot.press("enter")
                await pilot.pause()

            self.assertEqual(app.selected_session_id, expected)

    async def test_modal_escape_does_not_leave_stacked_palettes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            store.create()
            store.create()
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.press("ctrl+p")
                await pilot.pause()
                self.assertIsInstance(app.screen, CommandPalette)
                await pilot.press("escape")
                await pilot.pause()
                self.assertNotIsInstance(app.screen, CommandPalette)
                await pilot.press("tab")
                await pilot.pause()
                self.assertIsInstance(app.screen, SessionPalette)
                await pilot.press("j")
                await pilot.press("enter")
                await pilot.pause()
                self.assertNotIsInstance(app.screen, CommandPalette)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
