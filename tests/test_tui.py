from __future__ import annotations

import os
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from textual import events

from llm_browser.events import Event
from llm_browser.session.store import SessionStore
from llm_browser.tui.app import (
    BrowserUseTerminalApp,
    ComposerInput,
    CommandPalette,
    SecretInputScreen,
    SessionPalette,
    TextualTui,
    _artifact_kind,
    _artifact_paths,
    _compact_markdown_line_text,
    _compact_transcript_markdown_lines,
    _composer_visible_line_count,
    _current_tool,
    _dataset_run_id_from_path,
    _dataset_run_label,
    _dataset_task_id_from_path,
    _final_line,
    _file_links_for_text,
    _footer_hintbar_markup,
    _footer_hintbar_visible_width,
    _format_event_for_transcript,
    _format_events_for_log,
    _format_events_for_transcript,
    _linkify_markdown,
    _latest_image_line,
    _latest_browser_live_url,
    _normalize_browser_mode,
    _preview_text_for_artifact,
    _progress_bar,
    _rich_link,
    _short_task_list,
    _summarize_code,
    _summarize_task_text,
    _textual_mouse_enabled,
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

    def test_footer_hintbar_drops_low_priority_items_before_wrapping(self) -> None:
        items = [
            ("tab", "sessions", True),
            ("/", "settings", False),
            ("ctrl+p", "commands", False),
            ("f1", "keys", False),
            ("ctrl+q", "quit", False),
        ]

        rendered = _footer_hintbar_markup(items, width=76)

        self.assertIn("tab", rendered)
        self.assertIn("ctrl+p", rendered)
        self.assertNotIn("ctrl+q", rendered)
        self.assertLessEqual(_footer_hintbar_visible_width(items[:-1]), 74)

    def test_format_events_for_log_coalesces_model_deltas(self) -> None:
        events = [
            Event(type="model.delta", session_id="s1", payload={"text": "Hel"}),
            Event(type="model.delta", session_id="s1", payload={"text": "lo"}),
            Event(type="tool.started", session_id="s1", payload={"name": "python", "tool_call_id": "c1"}),
        ]

        lines = _format_events_for_log(events)

        self.assertEqual(lines[0], ("[s1] model: Hello", "model.delta"))
        self.assertIn("tool start: python", lines[1][0])

    def test_format_events_for_transcript_hides_lifecycle_and_marks_followup(self) -> None:
        events = [
            Event(type="session.created", session_id="s1", payload={}),
            Event(type="session.input", session_id="s1", payload={"text": "first task"}),
            Event(type="session.status", session_id="s1", payload={"status": "running"}),
            Event(type="session.input", session_id="s1", payload={"text": "second task", "resumed": True}),
        ]

        self.assertEqual(
            _format_events_for_transcript(events),
            [("first task", "session.input"), ("second task", "session.followup")],
        )

    def test_transcript_tool_events_show_compact_arguments_and_output(self) -> None:
        long_text = "a" * 400
        events = [
            Event(
                type="tool.started",
                session_id="s1",
                payload={
                    "name": "python",
                    "tool_call_id": "call_1",
                    "arguments": {"code": "print('hello')", "headless": True},
                },
            ),
            Event(
                type="tool.finished",
                session_id="s1",
                payload={"name": "python", "output": {"text": long_text, "data": {"ok": True}}},
            ),
        ]

        lines = _format_events_for_transcript(events, show_tools=True)

        self.assertIn("print('hello')", lines[0][0])
        self.assertIn("1 lines", lines[1][0])
        self.assertIn("400 B", lines[1][0])
        self.assertNotIn('"ok": true', lines[1][0])

    def test_default_transcript_hides_python_probe_tools(self) -> None:
        events = [
            Event(type="session.input", session_id="s1", payload={"text": "inspect the page"}),
            Event(
                type="tool.started",
                session_id="s1",
                payload={
                    "name": "python",
                    "tool_call_id": "call_1",
                    "arguments": {"code": "from llm_browser import *\nprint('ready')"},
                },
            ),
            Event(
                type="tool.finished",
                session_id="s1",
                payload={"name": "python", "tool_call_id": "call_1", "output": {"text": "ready"}},
            ),
            Event(type="model.delta", session_id="s1", payload={"text": "Ready.\n"}),
        ]

        lines = _format_events_for_transcript(events)
        rendered = "\n".join(line for line, _ in lines)

        self.assertEqual([event_type for _, event_type in lines], ["session.input", "model.delta"])
        self.assertNotIn("python", rendered)
        self.assertNotIn("llm_browser", rendered)

    def test_default_transcript_expands_browser_python_tools(self) -> None:
        events = [
            Event(
                type="tool.started",
                session_id="s1",
                payload={
                    "name": "python",
                    "arguments": {
                        "code": "\n".join(
                            [
                                "goto_url('https://example.com/search')",
                                "fill_input('#q', 'banana plugs')",
                                "click_text('Search')",
                                "capture_screenshot('results')",
                            ]
                        )
                    },
                },
            )
        ]

        lines = _format_events_for_transcript(events)

        self.assertEqual([event_type for _, event_type in lines], ["browser.action"] * 4)
        self.assertIn("browse open example.com/search", lines[0][0])
        self.assertIn("browse type #q", lines[1][0])
        self.assertIn('browse tap "Search"', lines[2][0])
        self.assertIn("browse snap results screenshot", lines[3][0])

    def test_python_code_summary_describes_intent_instead_of_dumping_first_line(self) -> None:
        self.assertEqual(
            _summarize_code("# but: extract Netflix country prices to JSON\nimport requests"),
            "extract Netflix country prices to JSON",
        )
        explicit_event = Event(
            type="tool.started",
            session_id="s1",
            payload={"name": "python", "arguments": {"code": "# but: parse visible article text\nprint('x')"}},
        )
        self.assertEqual(_format_event_for_transcript(explicit_event), "→ parse visible article text")
        self.assertEqual(
            _summarize_code("goto_url('https://help.netflix.com/en/node/24926')"),
            "open https://help.netflix.com/en/node/24926",
        )
        self.assertEqual(
            _summarize_code("import requests, bs4, re, json, time"),
            "prepare imports: requests, bs4, re, json, time",
        )
        self.assertEqual(
            _summarize_code("from browser import goto_url, wait_for_load, capture_screenshot, js"),
            "prepare browser helpers: nav, wait, screenshot, JS",
        )
        self.assertEqual(
            _summarize_code("print(js('document.documentElement.innerHTML'))"),
            "read page HTML",
        )
        self.assertEqual(
            _summarize_code("print(js(\"document.querySelector('select').outerHTML\"))"),
            "inspect page DOM",
        )
        scraper = """
import requests, bs4, json
html = requests.get(url).text
for country in countries:
    prices[country] = extract_price(country, html)
json.dump(prices, open('/tmp/netflix_prices.json', 'w'))
        """
        self.assertEqual(_summarize_code(scraper), "5 lines · extract country pricing to JSON")

    def test_browser_python_tool_starts_render_as_browser_actions(self) -> None:
        navigate_event = Event(
            type="tool.started",
            session_id="s1",
            payload={"name": "python", "arguments": {"code": "goto_url('https://help.netflix.com/en/node/24926')"}},
        )
        self.assertEqual(
            _format_event_for_transcript(navigate_event),
            "browse open help.netflix.com/en/node/24926",
        )

        action_event = Event(
            type="tool.started",
            session_id="s1",
            payload={
                "name": "python",
                "arguments": {
                    "code": "\n".join(
                        [
                            "goto_url('https://example.com/search')",
                            "fill_input('#q', 'banana plugs')",
                            "click_text('Search')",
                            "capture_screenshot('results')",
                        ]
                    )
                },
            },
        )
        formatted = _format_event_for_transcript(action_event)

        self.assertEqual(formatted, "browse steps 4 browser moves · open, type, tap, snap")
        self.assertNotIn("python", formatted)

    def test_done_transcript_preserves_markdown(self) -> None:
        event = Event(type="session.done", session_id="s1", payload={"result": "**Done**\n- item"})

        self.assertEqual(_format_event_for_transcript(event), "**Done**\n- item")

    def test_failed_transcript_compacts_provider_json_error(self) -> None:
        event = Event(
            type="session.failed",
            session_id="s1",
            payload={
                "error": (
                    'Codex Responses request failed: HTTP 400: { "error": { '
                    '"message": "No tool call found for function call output.", '
                    '"type": "invalid_request_error" } }'
                )
            },
        )

        formatted = _format_event_for_transcript(event)

        self.assertEqual(
            formatted,
            "failed: Codex Responses request failed: HTTP 400: No tool call found for function call output.",
        )
        self.assertNotIn("invalid_request_error", formatted)

    def test_transcript_dedupes_streamed_final_answer(self) -> None:
        events = [
            Event(type="model.delta", session_id="s1", payload={"text": "The file is here:\n\n"}),
            Event(type="model.delta", session_id="s1", payload={"text": "`/tmp/result.md`"}),
            Event(type="session.status", session_id="s1", payload={"status": "done"}),
            Event(type="session.done", session_id="s1", payload={"result": "The file is here:\n\n`/tmp/result.md`"}),
        ]

        lines = _format_events_for_transcript(events)

        self.assertEqual([event_type for _, event_type in lines], ["model.delta", "model.delta"])
        self.assertNotIn("session.done", [event_type for _, event_type in lines])

    def test_transcript_dedupes_token_streamed_markdown_final_answer(self) -> None:
        result = (
            "For your 5.1 speaker setup:\n\n"
            "- You need **2 banana plugs per speaker wire run** if you only use them on the AVR side.\n"
            "- You need **4 banana plugs per speaker wire run** if you use them on both AVR and speaker ends."
        )
        events = [
            Event(type="model.delta", session_id="s1", payload={"text": "For your 5.1 speaker setup:\n\n"}),
            Event(type="model.delta", session_id="s1", payload={"text": "- You need **2 banana"}),
            Event(type="model.delta", session_id="s1", payload={"text": " plugs per speaker wire run** if"}),
            Event(type="model.delta", session_id="s1", payload={"text": " you only use them on the AVR side.\n"}),
            Event(type="model.delta", session_id="s1", payload={"text": "- You need **4 banana plugs per speaker wire run**"}),
            Event(type="model.delta", session_id="s1", payload={"text": " if you use them on both AVR and speaker ends."}),
            Event(type="session.status", session_id="s1", payload={"status": "done"}),
            Event(type="session.done", session_id="s1", payload={"result": result}),
        ]

        lines = _format_events_for_transcript(events)

        self.assertNotIn("session.done", [event_type for _, event_type in lines])

    def test_compact_markdown_keeps_inline_spaces(self) -> None:
        rendered = _compact_markdown_line_text(
            "- You need **2 banana plugs per speaker wire run** if you only use them."
        )

        self.assertEqual(
            rendered.plain.replace("\u00a0", " "),
            "• You need 2 banana plugs per speaker wire run if you only use them.",
        )
        bold_segments = [
            rendered.plain[span.start : span.end]
            for span in rendered.spans
            if "bold" in str(span.style)
        ]
        self.assertIn("2 banana plugs per speaker wire run\u00a0", bold_segments)

    def test_compact_markdown_collapses_blank_lines_between_list_items(self) -> None:
        lines = _compact_transcript_markdown_lines("- one\n\n- two\n\nParagraph\n\nNext")

        self.assertEqual(lines, ["- one", "- two", "", "Paragraph", "", "Next"])

    def test_done_tool_result_is_not_rendered_as_extra_final_answer(self) -> None:
        event = Event(
            type="tool.finished",
            session_id="s1",
            payload={"name": "done", "output": {"text": "same final answer"}},
        )

        self.assertEqual(_format_event_for_transcript(event), "")

    def test_linkify_markdown_autolinks_urls_and_existing_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "notes.md"
            path.write_text("# Notes\n", encoding="utf-8")

            linked = _linkify_markdown(f"See https://example.com and {path}.")

            self.assertIn("<https://example.com>", linked)
            self.assertEqual(_file_links_for_text(str(path)), [path.resolve()])

    def test_linkify_markdown_keeps_long_links_clickable_and_inline_file_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / ("very_" + "long_" * 18 + "notes.md")
            path.write_text("# Notes\n", encoding="utf-8")
            url = "https://live.browser-use.com/?wss=" + ("abc123" * 24)

            linked = _linkify_markdown(f"browser live preview:\n{url}\n`{path}`")

            self.assertIn(f"<{url}>", linked)
            self.assertNotIn("file://", linked)
            self.assertEqual(_file_links_for_text(f"`{path}`"), [path.resolve()])

    def test_browser_live_preview_event_renders_compact_link(self) -> None:
        event = Event(
            type="browser.live_url",
            session_id="s1",
            payload={"live_url": "https://live.example/session"},
        )

        formatted = _format_event_for_transcript(event)

        self.assertEqual(formatted, "browse live live.example/session")

    def test_session_followup_renders_text_not_raw_payload(self) -> None:
        event = Event(type="session.followup", session_id="s1", payload={"text": "where is the file?"})

        formatted = _format_event_for_transcript(event)

        self.assertEqual(formatted, "where is the file?")

    def test_transcript_dedupes_streamed_tool_output_from_finished_event(self) -> None:
        text = "line 1\nline 2"
        events = [
            Event(
                type="tool.output",
                session_id="s1",
                payload={"name": "shell", "stream": "stdout", "tool_call_id": "call_1", "text": text},
            ),
            Event(
                type="tool.finished",
                session_id="s1",
                payload={"name": "shell", "tool_call_id": "call_1", "output": {"text": text, "data": {"ok": True}}},
            ),
        ]

        lines = _format_events_for_transcript(events, show_tools=True)

        self.assertEqual(len(lines), 1)
        self.assertEqual(lines[0][1], "tool.output")
        self.assertIn("line 1", lines[0][0])

    def test_tool_output_preview_decodes_json_payload_instead_of_storage_format(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "tool-output" / "call_1_python.txt"
            path.parent.mkdir()
            path.write_text(json.dumps(["short", "Zurich\\nSan Francisco\\n$643"]) + "\n\ndata={'ok': True}", encoding="utf-8")

            text, mode = _preview_text_for_artifact(path)

            self.assertEqual(mode, "plain")
            self.assertIn("Zurich\nSan Francisco", text)
            self.assertNotIn("data=", text)

    def test_tool_output_preview_detects_markdown_payload(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "tool-output" / "call_1_python.txt"
            path.parent.mkdir()
            path.write_text(json.dumps(["| A | B |\\n|---|---|\\n| 1 | 2 |"]) + "\n\ndata={'ok': True}", encoding="utf-8")

            text, mode = _preview_text_for_artifact(path)

            self.assertEqual(mode, "markdown")
            self.assertIn("| A | B |", text)

    def test_normalize_browser_mode_accepts_user_facing_aliases(self) -> None:
        self.assertEqual(_normalize_browser_mode("real"), "real")
        self.assertEqual(_normalize_browser_mode("remote"), "cloud")
        self.assertEqual(_normalize_browser_mode("chromium"), "chromium")
        self.assertIsNone(_normalize_browser_mode("netscape"))

    def test_composer_visible_line_count_wraps_and_caps_at_five(self) -> None:
        self.assertEqual(_composer_visible_line_count("", 80), 1)
        self.assertEqual(_composer_visible_line_count("x" * 100, 52), 2)
        self.assertEqual(_composer_visible_line_count("\n".join(str(i) for i in range(10)), 80), 5)

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

    def test_final_line_compacts_failed_session_errors(self) -> None:
        events = [
            Event(
                type="session.failed",
                session_id="s1",
                payload={
                    "error": (
                        'Codex Responses request failed: HTTP 400: { "error": { '
                        '"message": "No tool call found for function call output.", '
                        '"type": "invalid_request_error" } }'
                    )
                },
            )
        ]

        self.assertEqual(
            _final_line(events),
            "failed: Codex Responses request failed: HTTP 400: No tool call found for function call output.",
        )

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

    def test_live_browser_url_helpers_render_clickable_link(self) -> None:
        events = [
            Event(type="browser.live_url", session_id="s1", payload={"live_url": "https://live.example/session"}),
        ]

        self.assertEqual(_latest_browser_live_url(events), "https://live.example/session")
        self.assertEqual(
            _rich_link("open live preview", "https://live.example/session"),
            "[link=\"https://live.example/session\"][#8aa6a0]open live preview[/][/link]",
        )

    def test_artifact_kind_prioritizes_trace_and_download_dirs(self) -> None:
        self.assertEqual(_artifact_kind(Path("/tmp/session/browser/traces/001_trace.json")), "trace")
        self.assertEqual(_artifact_kind(Path("/tmp/session/browser/downloads/report.csv")), "download")

    def test_artifact_paths_hide_internal_compactions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            workspace = Path(tmp) / "workspace"
            workspace.mkdir()
            session = store.create(cwd=workspace)
            internal = session.artifact_dir / "compactions" / "001.json"
            internal.parent.mkdir(parents=True)
            internal.write_text("{}", encoding="utf-8")
            visible = session.artifact_dir / "result.md"
            visible.write_text("# Result", encoding="utf-8")

            self.assertEqual(_artifact_paths(session), [visible])

    def test_artifact_paths_prioritize_user_outputs_over_screenshot_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            workspace = Path(tmp) / "workspace"
            workspace.mkdir()
            session = store.create(cwd=workspace)
            screenshot = session.artifact_dir / "browser" / "screenshots" / "001_screenshot.png"
            screenshot.parent.mkdir(parents=True)
            screenshot.write_bytes(b"png")
            screenshot.with_suffix(".json").write_text("{}", encoding="utf-8")
            note = workspace / "answer.md"
            note.write_text("# Answer", encoding="utf-8")

            paths = _artifact_paths(session)

            self.assertEqual(paths[:2], [note.resolve(), screenshot])
            self.assertNotIn(screenshot.with_suffix(".json"), paths)


class TuiInteractionTest(unittest.IsolatedAsyncioTestCase):
    async def test_tui_starts_without_selecting_existing_session(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            store.create()
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.pause()

            self.assertIsNone(app.selected_session_id)

    async def test_session_inspector_is_hidden_by_default_and_toggles_with_f2(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "inspect me"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                await pilot.pause()
                sidebar = app.query_one("#sidebar")

                self.assertFalse(sidebar.display)
                await pilot.press("f2")
                await pilot.pause()
                self.assertTrue(sidebar.display)
                await pilot.press("f2")
                await pilot.pause()
                self.assertFalse(sidebar.display)

    async def test_session_log_loads_full_history_past_event_tail(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "first visible request"})
            for index in range(430):
                store.emit(
                    session.id,
                    "tool.started",
                    {
                        "name": "python",
                        "tool_call_id": f"probe_{index}",
                        "arguments": {"code": "from llm_browser import *\nprint('ready')"},
                    },
                )
            store.emit(session.id, "model.delta", {"text": "final visible answer\n"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                await pilot.pause()
                transcript = app.query_one("#transcript")

                visual = "\n".join(line.text for line in transcript.lines)
                self.assertIn("first visible request", visual)
                self.assertIn("final visible answer", visual)

    async def test_loaded_session_prompt_strip_uses_transcript_width(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "how many banana plugs do i actually need?"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                await pilot.pause()
                transcript = app.query_one("#transcript")

                self.assertGreaterEqual(transcript.lines[0].cell_length, 100)

    async def test_loaded_session_separates_prompt_actions_and_answer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "go to google"})
            store.emit(
                session.id,
                "tool.started",
                {
                    "name": "python",
                    "arguments": {"code": "goto_url('https://www.google.com')"},
                },
            )
            store.emit(session.id, "model.delta", {"text": "Opened google.com.\n"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                await pilot.pause()
                transcript = app.query_one("#transcript")

                lines = [line.text.rstrip("\u00a0") for line in transcript.lines]
                prompt_index = next(index for index, line in enumerate(lines) if "go to google" in line)
                action_index = next(index for index, line in enumerate(lines) if "www.google.com" in line)
                answer_index = next(index for index, line in enumerate(lines) if "Opened google.com." in line)

                self.assertEqual(lines[prompt_index + 1], "")
                self.assertEqual(action_index, prompt_index + 2)
                self.assertEqual(lines[action_index + 1], "")
                self.assertEqual(answer_index, action_index + 2)

    async def test_artifact_shortcut_opens_inspector(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            artifact = session.artifact_dir / "result.md"
            artifact.write_text("# Result", encoding="utf-8")
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                await pilot.press("f3")
                await pilot.pause()

                self.assertTrue(app.query_one("#sidebar").display)
                self.assertEqual(app.selected_artifact_path, str(artifact))
                self.assertIs(app.focused, app.query_one("#command", ComposerInput))

    async def test_composer_ctrl_c_requires_second_press_to_quit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app._set_composer_text("draft task text")
                await pilot.pause()
                with patch.object(app, "exit") as exit_mock:
                    await pilot.press("ctrl+c")
                    await pilot.pause()

                    self.assertFalse(exit_mock.called)
                    self.assertEqual(app.query_one("#command", ComposerInput).text, "")
                    hintbar = str(app.query_one("#hintbar").visual)
                    self.assertIn("ctrl+c", hintbar)
                    self.assertIn("again to quit", hintbar)

                    await pilot.press("ctrl+c")
                    await pilot.pause()

                    exit_mock.assert_called_once()

    async def test_plain_q_types_into_composer_instead_of_quitting(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                with patch.object(app, "exit") as exit_mock:
                    await pilot.press("q")
                    await pilot.pause()

                    self.assertEqual(app.query_one("#command", ComposerInput).text, "q")
                    self.assertFalse(exit_mock.called)

    async def test_ctrl_q_quits_from_composer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                with patch.object(app, "exit") as exit_mock:
                    await pilot.press("ctrl+q")
                    await pilot.pause()

                    exit_mock.assert_called_once()

    async def test_empty_composer_arrow_scroll_targets_transcript(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "long task"})
            for index in range(80):
                store.emit(session.id, "model.delta", {"text": f"line {index}\n"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(90, 18)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                await pilot.pause()
                transcript = app.query_one("#transcript")
                transcript.scroll_end(animate=False, immediate=True)
                await pilot.pause()
                bottom = transcript.scroll_y

                self.assertGreater(bottom, 0)
                await pilot.press("up")
                await pilot.pause()
                self.assertLess(transcript.scroll_y, bottom)
                scrolled_up = transcript.scroll_y
                await pilot.press("down")
                await pilot.pause()
                self.assertGreater(transcript.scroll_y, scrolled_up)
                self.assertIs(app.focused, app.query_one("#command", ComposerInput))

    async def test_composer_mouse_wheel_scrolls_transcript_while_typing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "long task"})
            for index in range(80):
                store.emit(session.id, "model.delta", {"text": f"line {index}\n"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(90, 18)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                app._set_composer_text("one\ntwo\nthree\nfour\nfive\nsix")
                await pilot.pause()
                transcript = app.query_one("#transcript")
                command = app.query_one("#command", ComposerInput)
                transcript.scroll_end(animate=False, immediate=True)
                command.scroll_end(animate=False, immediate=True)
                await pilot.pause()
                bottom = transcript.scroll_y

                input_scroll = command.scroll_y
                self.assertGreater(bottom, 0)
                await pilot._post_mouse_events([events.MouseScrollUp], widget="#command")
                await pilot.pause()

                self.assertLess(transcript.scroll_y, bottom)
                self.assertEqual(command.scroll_y, input_scroll)
                self.assertIs(app.focused, command)

    async def test_composer_container_mouse_wheel_scrolls_transcript_while_typing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "long task"})
            for index in range(80):
                store.emit(session.id, "model.delta", {"text": f"line {index}\n"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(90, 18)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                app._set_composer_text("one\ntwo\nthree\nfour\nfive\nsix")
                await pilot.pause()
                transcript = app.query_one("#transcript")
                command = app.query_one("#command", ComposerInput)
                transcript.scroll_end(animate=False, immediate=True)
                command.scroll_end(animate=False, immediate=True)
                await pilot.pause()
                bottom = transcript.scroll_y
                input_scroll = command.scroll_y

                self.assertGreater(bottom, 0)
                await pilot._post_mouse_events([events.MouseScrollUp], widget="#composer")
                await pilot.pause()

                self.assertLess(transcript.scroll_y, bottom)
                self.assertEqual(command.scroll_y, input_scroll)
                self.assertIs(app.focused, command)

    async def test_composer_newline_keeps_existing_text_visible(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(90, 18)) as pilot:
                command = app.query_one("#command", ComposerInput)
                app._set_composer_text("almdlaslda")
                await pilot.pause()

                await pilot.press("ctrl+j")
                await pilot.pause()

                self.assertEqual(command.text, "almdlaslda\n")
                self.assertEqual(command.styles.height.value, 2)
                self.assertEqual(command.scroll_y, 0)

    async def test_transcript_follows_live_events_when_near_bottom(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "long task"})
            for index in range(90):
                store.emit(session.id, "model.delta", {"text": f"line {index}\n"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(90, 18)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                await pilot.pause()
                transcript = app.query_one("#transcript")
                transcript.scroll_end(animate=False, immediate=True)
                await pilot.pause()
                bottom = transcript.scroll_y

                self.assertGreater(bottom, 0)
                app._scroll_transcript(lines=-1)
                await pilot.pause()
                self.assertFalse(transcript.auto_scroll)
                self.assertLess(transcript.scroll_y, bottom)

                event = store.events.append(
                    Event(type="session.input", session_id=session.id, payload={"text": "follow up", "resumed": True})
                )
                app._handle_event(event)
                await pilot.pause()

                self.assertEqual(transcript.scroll_y, transcript.max_scroll_y)

                app._scroll_transcript(lines=-1)
                await pilot.pause()
                event = store.events.append(
                    Event(type="model.delta", session_id=session.id, payload={"text": "visible update near bottom\n"})
                )
                app._handle_event(event)
                await pilot.pause()

                self.assertEqual(transcript.scroll_y, transcript.max_scroll_y)

    async def test_transcript_preserves_position_when_far_from_bottom(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "long task"})
            for index in range(120):
                store.emit(session.id, "model.delta", {"text": f"line {index}\n"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(90, 18)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                await pilot.pause()
                transcript = app.query_one("#transcript")
                transcript.scroll_end(animate=False, immediate=True)
                await pilot.pause()
                bottom = transcript.scroll_y

                self.assertGreater(bottom, 8)
                app._scroll_transcript(lines=-4)
                await pilot.pause()
                scrolled_y = transcript.scroll_y
                self.assertLess(scrolled_y, bottom - app._transcript_follow_margin(transcript))

                event = store.events.append(
                    Event(type="model.delta", session_id=session.id, payload={"text": "visible update far away\n"})
                )
                app._handle_event(event)
                await pilot.pause()

                self.assertEqual(transcript.scroll_y, scrolled_y)
                self.assertLess(transcript.scroll_y, transcript.max_scroll_y)

    def test_textual_tui_disables_mouse_reporting_outside_tmux_for_terminal_selection(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tui = TextualTui(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            with patch.dict(os.environ, {}, clear=True), patch.object(tui.app, "run") as run_mock:
                self.assertEqual(tui.run(), 0)

            run_mock.assert_called_once_with(mouse=False)

    def test_textual_tui_enables_mouse_reporting_inside_tmux_for_scroll_wheel(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tui = TextualTui(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            with patch.dict(os.environ, {"TMUX": "/tmp/tmux"}, clear=True), patch.object(tui.app, "run") as run_mock:
                self.assertEqual(tui.run(), 0)

            run_mock.assert_called_once_with(mouse=True)

    def test_textual_tui_mouse_reporting_env_override(self) -> None:
        with patch.dict(os.environ, {"TMUX": "/tmp/tmux", "LLM_BROWSER_TUI_MOUSE": "0"}, clear=True):
            self.assertFalse(_textual_mouse_enabled())
        with patch.dict(os.environ, {"LLM_BROWSER_TUI_MOUSE": "1"}, clear=True):
            self.assertTrue(_textual_mouse_enabled())

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

    async def test_f1_opens_keyboard_shortcuts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.press("f1")
                await pilot.pause()

                self.assertIsInstance(app.screen, CommandPalette)
                self.assertEqual(app.screen.title_text, "Keyboard")

    async def test_question_mark_stays_in_composer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.press("?")
                await pilot.pause()

                self.assertNotIsInstance(app.screen, CommandPalette)
                self.assertEqual(app.query_one("#command", ComposerInput).text, "?")

    async def test_ctrl_u_keeps_text_area_line_editing_behavior(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app._set_composer_text("draft task text")
                await pilot.press("ctrl+u")
                await pilot.pause()

                self.assertEqual(app.query_one("#command", ComposerInput).text, "")

    async def test_option_delete_removes_previous_word_in_composer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app._set_composer_text("draft task text")
                await pilot.press("alt+backspace")
                await pilot.pause()

                self.assertEqual(app.query_one("#command", ComposerInput).text, "draft task ")

    async def test_ctrl_o_toggles_raw_tool_output(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                self.assertFalse(app._tool_details_visible)
                await pilot.press("ctrl+o")
                await pilot.pause()

                self.assertTrue(app._tool_details_visible)

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

    async def test_browser_mode_palette_sets_browser_env(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LLM_BROWSER_MODE": "auto"}, clear=False):
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/browser")
                await pilot.pause()
                self.assertIsInstance(app.screen, CommandPalette)
                await pilot.press("j")
                await pilot.press("enter")
                await pilot.pause()

            self.assertEqual(os.environ["LLM_BROWSER_MODE"], "chromium")

    async def test_slash_command_panel_opens_settings_from_composer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.press("/")
                await pilot.pause()
                panel = app.query_one("#slash-panel")
                self.assertTrue(panel.display)
                await pilot.press("down")
                await pilot.pause()
                self.assertEqual(panel.cursor_row, 1)
                await pilot.press("enter")
                await pilot.pause()
                self.assertIsInstance(app.screen, CommandPalette)

    async def test_composer_auto_expands_and_caps_at_five_rows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                command = app.query_one("#command", ComposerInput)
                app._set_composer_text("one\ntwo\nthree\nfour\nfive\nsix")
                await pilot.pause()

                self.assertEqual(command.styles.height.value, 5)
                self.assertEqual(app.query_one("#composer").styles.height.value, 7)

    async def test_home_runtime_meta_shows_model_provider_and_browser(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="codex", model_label="gpt-5.5")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.pause()
                runtime_meta = app.query_one("#runtime-meta")

                self.assertTrue(runtime_meta.display)
                visual = str(runtime_meta.visual)
                self.assertIn("gpt-5.5", visual)
                self.assertIn("codex", visual)
                self.assertIn("auto", visual)

    async def test_composer_enter_submits_and_clears_text(self) -> None:
        class FakeManager:
            def __init__(self, store: SessionStore) -> None:
                self.store = store
                self.started: list[str] = []

            def start(self, task: str):
                self.started.append(task)
                return self.store.create()

            def reap(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")
            fake_manager = FakeManager(store)
            app.manager = fake_manager  # type: ignore[assignment]

            async with app.run_test(size=(120, 36)) as pilot:
                app._set_composer_text("first line\nsecond line")
                await pilot.press("enter")
                await pilot.pause()

                self.assertEqual(fake_manager.started, ["first line\nsecond line"])
                self.assertEqual(app.query_one("#command", ComposerInput).text, "")

    async def test_multiline_task_is_not_parsed_as_command(self) -> None:
        class FakeManager:
            def __init__(self, store: SessionStore) -> None:
                self.store = store
                self.started: list[str] = []

            def start(self, task: str):
                self.started.append(task)
                return self.store.create()

            def reap(self) -> None:
                pass

        task = "\n".join(
            [
                "model the browser task as a real user workflow",
                "open the target page",
                "extract the visible prices",
                "save the result as JSON",
                "tell me where the file is",
            ]
        )
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")
            fake_manager = FakeManager(store)
            app.manager = fake_manager  # type: ignore[assignment]

            async with app.run_test(size=(120, 36)) as pilot:
                app._set_composer_text(task)
                await pilot.press("enter")
                await pilot.pause()

                self.assertEqual(fake_manager.started, [task])
                self.assertEqual(app.query_one("#command", ComposerInput).text, "")

    async def test_plain_prompt_with_unbalanced_quote_starts_task(self) -> None:
        class FakeManager:
            def __init__(self, store: SessionStore) -> None:
                self.store = store
                self.started: list[str] = []

            def start(self, task: str):
                self.started.append(task)
                return self.store.create()

            def reap(self) -> None:
                pass

        task = 'find the page titled "prices and save a screenshot'
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")
            fake_manager = FakeManager(store)
            app.manager = fake_manager  # type: ignore[assignment]

            async with app.run_test(size=(120, 36)) as pilot:
                app._set_composer_text(task)
                await pilot.press("enter")
                await pilot.pause()

                self.assertEqual(fake_manager.started, [task])
                self.assertEqual(app.query_one("#command", ComposerInput).text, "")

    async def test_ctrl_b_is_not_bound_to_browser_palette(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.press("ctrl+b")
                await pilot.pause()
                self.assertNotIsInstance(app.screen, CommandPalette)

    async def test_slash_settings_update_model_and_provider(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/provider codex")
                app._handle_command("/model gpt-5.4")
                await pilot.pause()

            self.assertEqual(app.provider_label, "codex")
            self.assertEqual(app.model_label, "gpt-5.4")

    async def test_provider_change_sets_provider_default_model(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="codex", model_label="gpt-5.5")

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/provider anthropic")
                await pilot.pause()

            self.assertEqual(app.provider_label, "anthropic")
            self.assertEqual(app.model_label, "claude-sonnet-4-6")

    async def test_model_palette_is_scoped_to_current_provider(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="anthropic", model_label="claude-sonnet-4-6")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.pause()
                rows = app._model_palette()

            labels = [row[0] for row in rows]
            commands = [row[1] for row in rows]
            self.assertIn("Claude Opus 4.7", labels)
            self.assertIn("provider", commands)
            self.assertNotIn("model gpt-5.5", commands)
            self.assertNotIn("model glm-5.1", commands)

    async def test_settings_palette_lists_provider_before_model(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="anthropic", model_label="claude-sonnet-4-6")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.pause()
                rows = app._settings_palette()

            self.assertEqual(rows[0][0], "Provider")
            self.assertEqual(rows[1][0], "Model")

    async def test_running_cloud_session_detail_renders_live_preview_link(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.emit(session.id, "session.input", {"text": "open a page"})
            store.update_status(session.id, "running")
            store.emit(session.id, "browser.live_url", {"live_url": "https://live.example/session"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._update_session_detail()
                await pilot.pause()
                detail = app.query_one("#session-detail")

            self.assertIn("open live preview", str(detail.visual))
            self.assertIn("https://live.example/session", str(detail.visual))

    async def test_running_cloud_session_shows_live_preview_activity_strip(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.update_status(session.id, "running")
            store.emit(session.id, "browser.live_url", {"live_url": "https://live.example/session"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._set_home_mode(False)
                app._update_activity_strip()
                await pilot.pause()
                activity = app.query_one("#activity")

                self.assertTrue(activity.display)
                self.assertIn("browsing", str(activity.visual))
                self.assertIn("live.example/session", str(activity.visual))

    async def test_done_session_detail_hides_finished_tool_noise(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.emit(session.id, "session.input", {"text": "make a table"})
            store.emit(session.id, "tool.started", {"name": "python", "tool_call_id": "call_1"})
            store.emit(session.id, "tool.finished", {"name": "python", "tool_call_id": "call_1", "output": {"text": "intermediate"}})
            store.update_status(session.id, "done")
            store.emit(session.id, "session.done", {"result": "| A | B |\n|---|---|\n| 1 | 2 |"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._update_session_detail()
                await pilot.pause()
                detail = app.query_one("#session-detail")

            self.assertIn("answer", str(detail.visual))
            self.assertIn("table: 1 row", str(detail.visual))
            self.assertNotIn("python done", str(detail.visual))

    async def test_session_detail_shows_token_usage_breakdown(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.emit(session.id, "session.input", {"text": "inspect usage"})
            store.emit(
                session.id,
                "model.usage",
                {
                    "provider": "openai",
                    "model": "gpt-5.5",
                    "usage": {
                        "input_tokens": 1000,
                        "input_total_tokens": 1100,
                        "output_tokens": 200,
                        "reasoning_tokens": 50,
                        "cache_read_tokens": 100,
                        "cache_write_tokens": 0,
                        "total_tokens": 1350,
                    },
                    "cost_usd": 0.0123,
                },
            )
            store.update_status(session.id, "done")
            store.emit(session.id, "session.done", {"result": "complete"})
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._update_session_detail()
                await pilot.pause()
                detail = app.query_one("#session-detail")

            visual = str(detail.visual)
            self.assertIn("usage", visual)
            self.assertIn("$0.01", visual)
            self.assertIn("1,350 tokens", visual)
            self.assertIn("in 1,100", visual)
            self.assertIn("out 250", visual)
            self.assertIn("cache read 100", visual)

    async def test_markdown_artifact_preview_renders_markdown(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            note = session.artifact_dir / "result.md"
            note.write_text("**Flight**\n\n- [Google](https://google.com)\n", encoding="utf-8")
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")

            with patch("llm_browser.tui.app._write_markdown") as write_markdown:
                async with app.run_test(size=(120, 36)) as pilot:
                    app.selected_session_id = session.id
                    app._preview_artifact(str(note), force=True)
                    await pilot.pause()

            self.assertEqual(write_markdown.call_count, 1)
            self.assertIn("**Flight**", write_markdown.call_args.args[1])

    async def test_auth_command_persists_browser_use_api_key(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=False):
            config_path = Path(tmp) / "config.json"
            app = BrowserUseTerminalApp(
                SessionStore(Path(tmp) / "state"),
                provider_label="fake",
                model_label="fake-model",
                config_path=config_path,
            )

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/auth browser-use bu_test_key")
                await pilot.pause()

            config = json.loads(config_path.read_text(encoding="utf-8"))
            self.assertEqual(config["browser"]["cloud_api_key"], "bu_test_key")
            self.assertEqual(os.environ["BROWSER_USE_API_KEY"], "bu_test_key")

    async def test_auth_command_persists_provider_api_key_to_auth_store(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(
            os.environ,
            {"LLM_BROWSER_PROVIDER_AUTH_PATH": str(Path(tmp) / "provider-auth.json")},
            clear=False,
        ):
            app = BrowserUseTerminalApp(SessionStore(Path(tmp) / "state"), provider_label="openrouter", model_label="glm-5.1")

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/auth openrouter or_test_key")
                await pilot.pause()

            auth_payload = json.loads((Path(tmp) / "provider-auth.json").read_text(encoding="utf-8"))
            self.assertEqual(auth_payload["openrouter"]["type"], "api_key")
            self.assertEqual(auth_payload["openrouter"]["key"], "or_test_key")
            self.assertEqual(os.environ["LLM_BROWSER_OPENROUTER_API_KEY"], "or_test_key")

    async def test_auth_without_args_opens_auth_palette(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="openrouter", model_label="glm-5.1")

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/auth")
                await pilot.pause()

                self.assertIsInstance(app.screen, CommandPalette)

    async def test_auth_api_key_without_value_opens_secret_prompt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="openrouter", model_label="glm-5.1")

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/auth api-key openrouter")
                await pilot.pause()

                self.assertIsInstance(app.screen, SecretInputScreen)

    async def test_auth_secret_prompt_consumes_key_submission(self) -> None:
        class FakeManager:
            def __init__(self) -> None:
                self.started: list[str] = []

            def start(self, task: str):
                self.started.append(task)
                raise AssertionError(f"unexpected task start: {task}")

            def reap(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as tmp, patch.dict(
            os.environ,
            {"LLM_BROWSER_PROVIDER_AUTH_PATH": str(Path(tmp) / "provider-auth.json")},
            clear=False,
        ):
            app = BrowserUseTerminalApp(SessionStore(Path(tmp) / "state"), provider_label="openrouter", model_label="glm-5.1")
            fake_manager = FakeManager()
            app.manager = fake_manager  # type: ignore[assignment]

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/auth api-key openrouter")
                await pilot.pause()
                await pilot.press("o", "r", "-", "t", "e", "s", "t", "enter")
                await pilot.pause()
                composer_text = app.query_one("#command", ComposerInput).text

            auth_payload = json.loads((Path(tmp) / "provider-auth.json").read_text(encoding="utf-8"))
            self.assertEqual(auth_payload["openrouter"]["key"], "or-test")
            self.assertEqual(fake_manager.started, [])
            self.assertEqual(composer_text, "")

    async def test_pending_auth_secret_handles_composer_submission_fallback(self) -> None:
        class FakeManager:
            def __init__(self) -> None:
                self.started: list[str] = []

            def start(self, task: str):
                self.started.append(task)
                raise AssertionError(f"unexpected task start: {task}")

            def reap(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as tmp, patch.dict(
            os.environ,
            {"LLM_BROWSER_PROVIDER_AUTH_PATH": str(Path(tmp) / "provider-auth.json")},
            clear=False,
        ):
            app = BrowserUseTerminalApp(SessionStore(Path(tmp) / "state"), provider_label="openrouter", model_label="glm-5.1")
            fake_manager = FakeManager()
            app.manager = fake_manager  # type: ignore[assignment]

            async with app.run_test(size=(120, 36)) as pilot:
                app._handle_command("/auth api-key openrouter")
                await pilot.pause()
                app.query_one("#command", ComposerInput).load_text("or-fallback")
                app.submit_composer()
                await pilot.pause()

            auth_payload = json.loads((Path(tmp) / "provider-auth.json").read_text(encoding="utf-8"))
            self.assertEqual(auth_payload["openrouter"]["key"], "or-fallback")
            self.assertEqual(fake_manager.started, [])

    async def test_auth_palette_includes_openrouter_and_claude_code_login(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            app = BrowserUseTerminalApp(SessionStore(Path(tmp)), provider_label="openrouter", model_label="glm-5.1")

            async with app.run_test(size=(120, 36)) as pilot:
                await pilot.pause()
                rows = app._auth_palette()

            commands = [row[1] for row in rows]
            self.assertIn("auth api-key openrouter", commands)
            self.assertIn("auth anthropic login", commands)
            self.assertIn("auth codex login", commands)

    async def test_plain_text_with_selected_done_session_resumes_in_place(self) -> None:
        class FakeManager:
            def __init__(self, store: SessionStore) -> None:
                self.store = store
                self.resumed: list[tuple[str, str]] = []

            def resume(self, session_id: str, instruction: str):
                self.resumed.append((session_id, instruction))
                self.store.emit(session_id, "session.input", {"text": instruction, "resumed": True})
                return self.store.load(session_id)

            def start(self, task: str):
                raise AssertionError(f"unexpected new task: {task}")

            def cancel(self, session_id: str) -> None:
                pass

            def reap(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.emit(session.id, "session.input", {"text": "first task"})
            store.update_status(session.id, "done")
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")
            fake_manager = FakeManager(store)
            app.manager = fake_manager  # type: ignore[assignment]

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app._load_session_log(session.id)
                app._handle_command("second task")
                await pilot.pause()

            self.assertEqual(fake_manager.resumed, [(session.id, "second task")])
            self.assertEqual(app.selected_session_id, session.id)

    async def test_cancel_shortcut_ignores_completed_session(self) -> None:
        class FakeManager:
            def __init__(self) -> None:
                self.cancelled: list[str] = []

            def cancel(self, session_id: str) -> None:
                self.cancelled.append(session_id)

            def reap(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create()
            store.update_status(session.id, "done")
            app = BrowserUseTerminalApp(store, provider_label="fake", model_label="fake-model")
            fake_manager = FakeManager()
            app.manager = fake_manager  # type: ignore[assignment]

            async with app.run_test(size=(120, 36)) as pilot:
                app.selected_session_id = session.id
                app.action_cancel_selected()
                await pilot.pause()

            self.assertEqual(fake_manager.cancelled, [])
            self.assertFalse((Path(tmp) / "sessions" / session.id / "cancel.json").exists())


if __name__ == "__main__":
    raise SystemExit(unittest.main())
