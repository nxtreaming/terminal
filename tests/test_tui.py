from __future__ import annotations

import unittest

from llm_browser.events import Event
from llm_browser.tui.app import _format_events_for_log, _summarize_task_text
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


if __name__ == "__main__":
    raise SystemExit(unittest.main())
