from __future__ import annotations

import json
import os
import queue
import re
import shlex
import subprocess
import threading
import time
from pathlib import Path
from typing import Callable, Optional

from rich.markup import escape
from rich.text import Text
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import DataTable, Input, RichLog, Static

from llm_browser.agent import SessionManager
from llm_browser.browser import browser_runtime_diagnostics
from llm_browser.brand import PRODUCT_NAME
from llm_browser.config import redacted_config
from llm_browser.datasets import build_dataset_prompt, load_dataset, load_manifest, select_tasks, summarize_manifest
from llm_browser.events import Event
from llm_browser.provider.base import Provider
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tui.simple import format_event


ProviderFactory = Callable[[], Optional[Provider]]


COMMAND_PALETTE: list[tuple[str, str, str]] = [
    ("New task", "", "Type a plain request and press enter"),
    ("Dataset sample", "dataset real_v8 1", "Run one real_v8 dataset task"),
    ("Dataset by task id", "dataset real_v8 --task-id ", "Start a specific dataset task"),
    ("Resume selected", "resume", "Continue from the selected session"),
    ("Cancel selected", "cancel", "Interrupt the selected session"),
    ("Trace selected", "trace", "Write a trace bundle artifact"),
    ("Self eval", "eval", "Start a self-evaluation child session"),
    ("Report run", "report", "Summarize the selected dataset run"),
    ("Open artifact", "open", "Open the selected artifact"),
    ("Refresh", "refresh", "Reload sessions and artifacts"),
    ("Clear transcript", "clear", "Clear the visible transcript"),
    ("Browser config", "browser", "Show browser runtime details"),
    ("Auth status", "auth", "Show provider authentication status"),
    ("Config", "config", "Show redacted app configuration"),
]


class CommandPalette(ModalScreen[Optional[str]]):
    CSS = """
    CommandPalette {
        align: center middle;
        background: #000000 70%;
    }

    #palette {
        width: 72;
        max-width: 96%;
        max-height: 70%;
        padding: 2 3 1 3;
        background: #141414;
    }

    #palette-head {
        height: 1;
        margin-bottom: 1;
    }

    #palette-title {
        width: 1fr;
        color: #eeeeee;
        text-style: bold;
    }

    #palette-esc {
        width: auto;
        color: #808080;
    }

    #palette-filter {
        height: 1;
        margin-bottom: 1;
        background: #141414;
        color: #eeeeee;
        border: none;
    }

    #palette-table {
        height: 1fr;
        background: #141414;
        color: #eeeeee;
    }
    """

    BINDINGS = [("escape", "close", "Close"), ("ctrl+c", "close", "Close")]

    def __init__(self, commands: list[tuple[str, str, str]]) -> None:
        super().__init__()
        self.commands = commands

    def compose(self) -> ComposeResult:
        with Container(id="palette"):
            with Horizontal(id="palette-head"):
                yield Static("Commands", id="palette-title")
                yield Static("esc", id="palette-esc")
            yield Input(placeholder="Search commands", id="palette-filter", compact=True)
            table = DataTable(
                id="palette-table",
                cursor_type="row",
                show_header=False,
                show_row_labels=False,
                cell_padding=1,
            )
            table.add_columns("name", "description")
            yield table

    def on_mount(self) -> None:
        self._populate("")
        self.query_one("#palette-filter", Input).focus()

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "palette-filter":
            self._populate(event.value)

    def on_input_submitted(self, event: Input.Submitted) -> None:
        value = event.value.strip()
        table = self.query_one("#palette-table", DataTable)
        if table.row_count:
            self.dismiss(str(table.get_row_at(table.cursor_row)[0]))
        elif value:
            self.dismiss(value)

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        self.dismiss(str(event.row_key.value))

    def action_close(self) -> None:
        self.dismiss(None)

    def _populate(self, query: str) -> None:
        table = self.query_one("#palette-table", DataTable)
        table.clear()
        needle = query.strip().lower()
        for title, command, description in self.commands:
            searchable = f"{title} {command} {description}".lower()
            if needle and needle not in searchable:
                continue
            key = command or "run "
            table.add_row(key, Text(description, style="#808080"), key=key)


class SessionPalette(ModalScreen[Optional[str]]):
    CSS = """
    SessionPalette {
        align: center middle;
        background: #000000 70%;
    }

    #sessions-dialog {
        width: 88;
        max-width: 96%;
        max-height: 70%;
        padding: 2 3 1 3;
        background: #141414;
    }

    #sessions-head {
        height: 1;
        margin-bottom: 1;
    }

    #sessions-dialog-title {
        width: 1fr;
        color: #eeeeee;
        text-style: bold;
    }

    #sessions-esc {
        width: auto;
        color: #808080;
    }

    #sessions-filter {
        height: 1;
        margin-bottom: 1;
        background: #141414;
        color: #eeeeee;
        border: none;
    }

    #sessions-table {
        height: 1fr;
        background: #141414;
        color: #eeeeee;
    }
    """

    BINDINGS = [("escape", "close", "Close"), ("ctrl+c", "close", "Close")]

    def __init__(self, rows: list[tuple[str, str, str, str, str]]) -> None:
        super().__init__()
        self.rows = rows
        self._visible_session_ids: list[str] = []

    def compose(self) -> ComposeResult:
        with Container(id="sessions-dialog"):
            with Horizontal(id="sessions-head"):
                yield Static("Sessions", id="sessions-dialog-title")
                yield Static("esc", id="sessions-esc")
            yield Input(placeholder="Search sessions", id="sessions-filter", compact=True)
            table = DataTable(
                id="sessions-table",
                cursor_type="row",
                show_header=False,
                show_row_labels=False,
                cell_padding=1,
            )
            table.add_columns("session", "state", "age", "run")
            yield table

    def on_mount(self) -> None:
        self._populate("")
        self.query_one("#sessions-filter", Input).focus()

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "sessions-filter":
            self._populate(event.value)

    def on_input_submitted(self, event: Input.Submitted) -> None:
        table = self.query_one("#sessions-table", DataTable)
        if table.row_count:
            self.dismiss(self._visible_session_ids[table.cursor_row])

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        self.dismiss(str(event.row_key.value))

    def action_close(self) -> None:
        self.dismiss(None)

    def _populate(self, query: str) -> None:
        table = self.query_one("#sessions-table", DataTable)
        table.clear()
        self._visible_session_ids = []
        needle = query.strip().lower()
        for session_id, status, age, run, task in self.rows:
            searchable = f"{session_id} {status} {age} {run} {task}".lower()
            if needle and needle not in searchable:
                continue
            self._visible_session_ids.append(session_id)
            title = f"[bold]{escape(task[:42] or session_id)}[/bold]\n[dim]{escape(session_id)}[/dim]"
            table.add_row(Text.from_markup(title), _status_text(status), age, run, key=session_id)


class BrowserUseTerminalApp(App[None]):
    CSS = """
    Screen {
        background: #0a0a0a;
        color: #eeeeee;
    }

    #body {
        height: 1fr;
    }

    #main {
        width: 1fr;
        min-width: 52;
        padding: 2 3 1 3;
        background: #0a0a0a;
    }

    #transcript {
        height: 1fr;
        background: #0a0a0a;
        color: #eeeeee;
        scrollbar-color: #606060;
        scrollbar-background: #0a0a0a;
    }

    #composer {
        height: 5;
        margin-top: 1;
        padding: 1 2 0 2;
        background: #1e1e1e;
        border-left: tall #5c9cf5;
    }

    #command {
        height: 1;
        border: none;
        background: #1e1e1e;
        color: #eeeeee;
    }

    #composer-meta {
        height: 1;
        margin-top: 1;
        color: #808080;
    }

    #hintbar {
        height: 1;
        color: #808080;
        padding: 0 1;
    }

    #sidebar {
        width: 42;
        min-width: 34;
        padding: 2 2 1 2;
        background: #141414;
    }

    #session-detail {
        height: auto;
        max-height: 18;
        color: #eeeeee;
        background: #141414;
    }

    #artifacts-title, #preview-title {
        height: 1;
        margin-top: 1;
        color: #eeeeee;
        text-style: bold;
    }

    #artifacts {
        height: 1fr;
        background: #141414;
        color: #eeeeee;
    }

    #artifact-preview {
        height: 10;
        margin-top: 1;
        background: #141414;
        color: #808080;
        scrollbar-color: #606060;
        scrollbar-background: #141414;
    }

    #sidebar-footer {
        height: 2;
        color: #808080;
        padding-top: 1;
    }

    DataTable {
        background: #141414;
        color: #eeeeee;
        scrollbar-color: #606060;
        scrollbar-background: #141414;
    }

    DataTable > .datatable--cursor {
        background: #282828;
        color: #eeeeee;
    }

    DataTable:focus > .datatable--cursor {
        background: #fab283;
        color: #0a0a0a;
        text-style: bold;
    }

    Input > .input--cursor {
        background: #eeeeee;
        color: #0a0a0a;
    }

    Input > .input--placeholder {
        color: #808080;
    }
    """

    BINDINGS = [
        Binding("escape", "cancel_selected", "Interrupt", priority=True),
        Binding("ctrl+c", "cancel_selected", "Interrupt", priority=True),
        Binding("tab", "show_sessions", "Sessions", priority=True),
        Binding("ctrl+p", "show_commands", "Commands", priority=True),
        Binding("ctrl+r", "refresh", "Refresh"),
        Binding("ctrl+l", "clear_log", "Clear"),
        Binding("o", "open_artifact", "Open"),
        Binding("q", "quit", "Quit"),
    ]

    def __init__(
        self,
        store: SessionStore,
        provider_factory: Optional[ProviderFactory] = None,
        max_turns: int = 80,
        provider_label: str = "fake",
        model_label: Optional[str] = None,
        config: Optional[dict] = None,
    ) -> None:
        super().__init__()
        self.store = store
        self.manager = SessionManager(store, provider_factory=provider_factory, max_turns=max_turns)
        self.provider_label = provider_label
        self.model_label = model_label
        self.config = config or {}
        self.selected_session_id: Optional[str] = None
        self.selected_artifact_path: Optional[str] = None
        self._preview_key: Optional[tuple[str, float, int]] = None
        self._model_buffers: dict[str, str] = {}
        self._stop = threading.Event()
        self._listener: Optional[threading.Thread] = None

    def compose(self) -> ComposeResult:
        with Horizontal(id="body"):
            with Vertical(id="main"):
                yield RichLog(id="transcript", wrap=True, highlight=True, markup=True)
                with Vertical(id="composer"):
                    yield Input(
                        placeholder='Ask anything... "Find the page and save a screenshot"',
                        id="command",
                        compact=True,
                    )
                    yield Static("", id="composer-meta")
                yield Static("", id="hintbar")
            with Vertical(id="sidebar"):
                yield Static("", id="session-detail")
                yield Static("artifacts", id="artifacts-title")
                artifacts = DataTable(
                    id="artifacts",
                    cursor_type="row",
                    show_header=False,
                    show_row_labels=False,
                    cell_padding=1,
                )
                artifacts.add_columns("kind", "name", "size")
                yield artifacts
                yield Static("preview", id="preview-title")
                yield RichLog(id="artifact-preview", wrap=True, highlight=True, markup=True)
                yield Static("", id="sidebar-footer")

    def on_mount(self) -> None:
        self.title = PRODUCT_NAME
        self.sub_title = "raw CDP browser agent"
        self.refresh_sessions()
        if self.selected_session_id:
            self._load_session_log(self.selected_session_id)
        else:
            self._write_home()
        self._update_statusbar()
        self._update_session_detail()
        self.query_one("#command", Input).focus()
        self._listener = threading.Thread(target=self._listen_events, name="browser-use-terminal-events", daemon=True)
        self._listener.start()
        self.set_interval(1.0, self._tick)

    def on_unmount(self) -> None:
        self._stop.set()

    def _listen_events(self) -> None:
        with self.store.bus.subscribe() as events:
            while not self._stop.is_set():
                try:
                    event = events.get(timeout=0.25)
                except queue.Empty:
                    continue
                try:
                    self.call_from_thread(self._handle_event, event)
                except RuntimeError:
                    return

    def _tick(self) -> None:
        self.manager.reap()
        self.refresh_sessions()
        self.refresh_artifacts()
        self._update_statusbar()
        self._update_session_detail()

    def _write_home(self) -> None:
        log = self.query_one("#transcript", RichLog)
        log.clear()
        log.write("")
        log.write("[bold #eeeeee]browser use terminal[/bold #eeeeee]")
        log.write("[#808080]Type a browser task below, or press [#eeeeee]ctrl+p[/] for commands.[/]")
        log.write("")
        log.write(f"[#5c9cf5]Build[/] [#808080]·[/] {escape(self.model_label or '-')} [#808080]{escape(self.provider_label)}[/]")

    def _write_banner(self) -> None:
        log = self.query_one("#transcript", RichLog)
        log.write("[bold #eeeeee]Commands[/bold #eeeeee]")
        log.write(
            "[#808080]Plain text starts a task. Slash commands include "
            "[#eeeeee]/dataset[/], [#eeeeee]/resume[/], [#eeeeee]/cancel[/], "
            "[#eeeeee]/trace[/], [#eeeeee]/eval[/], [#eeeeee]/report[/], "
            "[#eeeeee]/browser[/], and [#eeeeee]/open[/].[/]"
        )

    def _handle_event(self, event: Event) -> None:
        if self.selected_session_id is None:
            self.selected_session_id = event.session_id

        if event.type == "model.delta":
            if event.session_id == self.selected_session_id:
                self._append_model_delta(event)
            return

        if event.session_id == self.selected_session_id:
            self._flush_model_delta(event.session_id)
            self._write_log_line(_format_event_for_transcript(event), event.type)
        self.refresh_sessions()
        self.refresh_artifacts()
        self._update_statusbar()
        self._update_session_detail()

    def _append_model_delta(self, event: Event) -> None:
        text = str(event.payload.get("text") or "")
        if not text:
            return
        buffered = self._model_buffers.get(event.session_id, "") + text
        self._model_buffers[event.session_id] = buffered
        if "\n" in buffered or len(buffered) >= 700:
            self._flush_model_delta(event.session_id)

    def _flush_model_delta(self, session_id: str) -> None:
        text = self._model_buffers.pop(session_id, "")
        if not text.strip():
            return
        collapsed = " ".join(text.strip().split())
        self._write_log_line(collapsed, "model.delta")

    def _flush_all_model_deltas(self) -> None:
        for session_id in list(self._model_buffers):
            self._flush_model_delta(session_id)

    def _write_log_line(self, line: str, event_type: str) -> None:
        log = self.query_one("#transcript", RichLog)
        escaped = escape(line)
        if event_type == "session.input":
            log.write(f"[bold #5c9cf5]▌[/] [bold #eeeeee]Task[/bold #eeeeee]\n{escaped}")
        elif event_type == "session.created":
            log.write(f"[#808080]{escaped}[/]")
        elif event_type == "tool.started":
            log.write(f"[#808080]{escaped}[/]")
        elif event_type == "tool.failed":
            log.write(f"[#e06c75]{escaped}[/]")
        elif event_type == "tool.image":
            log.write(f"[#5c9cf5]{escaped}[/]")
        elif event_type == "tool.output":
            log.write(f"[#808080]{escaped}[/]")
        elif event_type == "tool.finished":
            log.write(f"[#808080]{escaped}[/]")
        elif event_type == "model.delta":
            log.write(f"[#eeeeee]{escaped}[/]")
        elif event_type in {"session.done", "session.cancelled"}:
            log.write(f"[#7fd88f]{escaped}[/]")
        elif event_type == "session.failed":
            log.write(f"[bold #e06c75]{escaped}[/bold #e06c75]")
        elif event_type == "session.deadline_warning":
            log.write(f"[#f5a742]{escaped}[/]")
        else:
            log.write(escaped)

    def on_input_submitted(self, event: Input.Submitted) -> None:
        line = event.value.strip()
        event.input.value = ""
        if not line:
            return
        self._handle_command(line)

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        if event.data_table.id == "artifacts":
            self.selected_artifact_path = str(event.row_key.value)
            self._preview_artifact(self.selected_artifact_path, force=True)

    def _handle_command(self, line: str) -> None:
        log = self.query_one("#transcript", RichLog)
        is_slash_command = line.startswith("/")
        normalized_line = line.lstrip("/")
        if normalized_line.startswith("run "):
            task = normalized_line[4:].strip()
            if not task:
                log.write("[#e06c75]run requires a task[/]")
                return
            self._start_task(task)
            return

        try:
            args = shlex.split(normalized_line)
        except ValueError as exc:
            log.write(f"[#e06c75]parse error: {escape(str(exc))}[/]")
            return
        if not args:
            return

        command = args[0]
        if command in {"quit", "exit"}:
            self.exit()
        elif command == "help":
            self._write_banner()
        elif command == "refresh":
            self.action_refresh()
        elif command == "clear":
            self.action_clear_log()
        elif command == "sessions":
            self.action_show_sessions()
        elif command == "artifacts":
            self.refresh_artifacts()
            log.write("[#7fd88f]artifacts refreshed[/]")
        elif command == "browser":
            log.write(escape(_browser_runtime_detail()))
        elif command == "config":
            log.write(escape(json.dumps(redacted_config(self.config), indent=2)))
        elif command == "auth":
            from llm_browser.auth import auth_status

            log.write(
                escape(
                    json.dumps(
                        {
                            "codex": auth_status(),
                            "openai_api_key": bool(os.environ.get("LLM_BROWSER_OPENAI_API_KEY") or os.environ.get("OPENAI_API_KEY")),
                        },
                        indent=2,
                    )
                )
            )
        elif command == "report":
            run_id = args[1] if len(args) >= 2 else self._selected_dataset_run_id()
            if not run_id:
                log.write("[#e06c75]report requires a run id or a selected dataset session[/]")
                return
            self._write_dataset_report(run_id)
        elif command == "show" and len(args) == 2:
            self.selected_session_id = args[1]
            self._load_session_log(args[1])
            self.refresh_artifacts()
        elif command == "resume":
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            instruction = " ".join(args[2:]) if len(args) > 2 else "Continue from the previous session state."
            if not session_id:
                log.write("[#e06c75]no selected session to resume[/]")
                return
            parent = self.store.load(session_id)
            if parent is None:
                log.write(f"[#e06c75]session not found: {escape(session_id)}[/]")
                return
            resumed = self.manager.start(instruction, parent_id=parent.id)
            self.selected_session_id = resumed.id
            self._load_session_log(resumed.id)
        elif command == "cancel":
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            if not session_id:
                log.write("[#e06c75]no selected session to cancel[/]")
                return
            self.manager.cancel(session_id)
            log.write(f"[#f5a742]cancel requested for {escape(session_id)}[/]")
        elif command == "open":
            path = args[1] if len(args) > 1 else self.selected_artifact_path
            self._open_artifact(path)
        elif command == "trace":
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            self._write_trace(session_id)
        elif command in {"eval", "self-eval"}:
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            self._start_self_eval(session_id)
        elif command == "dataset" and len(args) >= 2:
            count = 1
            task_ids: list[str] = []
            rest = args[2:]
            index = 0
            while index < len(rest):
                if rest[index] == "--task-id" and index + 1 < len(rest):
                    task_ids.append(rest[index + 1])
                    index += 2
                elif rest[index] == "--all":
                    count = len(load_dataset(args[1]))
                    index += 1
                else:
                    try:
                        count = int(rest[index])
                    except ValueError:
                        log.write(f"[#e06c75]invalid dataset option: {escape(rest[index])}[/]")
                        return
                    index += 1
            tasks = select_tasks(load_dataset(args[1]), count=count, task_ids=task_ids or None)
            for task in tasks:
                session = self.manager.start(build_dataset_prompt(task, headless=_browser_headless_default()))
                self.selected_session_id = session.id
                self._load_session_log(session.id)
        else:
            if is_slash_command:
                log.write(f"[#e06c75]unknown command: {escape(command)}[/]")
            else:
                self._start_task(line)

    def _load_session_log(self, session_id: str) -> None:
        log = self.query_one("#transcript", RichLog)
        log.clear()
        session = self.store.load(session_id)
        if session is None:
            log.write(f"[#e06c75]session not found: {escape(session_id)}[/]")
            return
        self.selected_session_id = session.id
        self.selected_artifact_path = None
        for line, event_type in _format_events_for_transcript(self.store.events.read(session.id)[-400:]):
            self._write_log_line(line, event_type)
        self._update_session_detail()

    def refresh_sessions(self) -> None:
        if self.selected_session_id is None:
            sessions = self.store.list()
            if sessions:
                self.selected_session_id = sessions[0].id
        self._update_statusbar()

    def refresh_artifacts(self) -> None:
        table = self.query_one("#artifacts", DataTable)
        table.clear()
        session_id = self.selected_session_id
        if not session_id:
            self.selected_artifact_path = None
            self._preview_artifact(None)
            return
        session = self.store.load(session_id)
        if session is None:
            self.selected_artifact_path = None
            self._preview_artifact(None)
            return
        first_path: Optional[str] = None
        if self.selected_artifact_path is not None and not Path(self.selected_artifact_path).exists():
            self.selected_artifact_path = None
        for path in _artifact_paths(session):
            if first_path is None:
                first_path = str(path)
            stat = path.stat()
            table.add_row(
                _artifact_kind(path),
                path.name,
                _format_bytes(stat.st_size),
                key=str(path),
            )
        if self.selected_artifact_path is None:
            self.selected_artifact_path = first_path
        self._preview_artifact(self.selected_artifact_path)

    def action_cancel_selected(self) -> None:
        if self.selected_session_id:
            self.manager.cancel(self.selected_session_id)

    def action_refresh(self) -> None:
        self.refresh_sessions()
        self.refresh_artifacts()

    def action_clear_log(self) -> None:
        self._model_buffers.clear()
        self.query_one("#transcript", RichLog).clear()

    def action_open_artifact(self) -> None:
        self._open_artifact(self.selected_artifact_path)

    def action_show_commands(self) -> None:
        def selected(command: Optional[str]) -> None:
            if command is None:
                self.query_one("#command", Input).focus()
                return
            command_input = self.query_one("#command", Input)
            if command.endswith(" "):
                command_input.value = command
                command_input.focus()
                return
            self._handle_command(command)
            command_input.focus()

        self.push_screen(CommandPalette(COMMAND_PALETTE), selected)

    def action_show_sessions(self) -> None:
        def selected(session_id: Optional[str]) -> None:
            if not session_id:
                self.query_one("#command", Input).focus()
                return
            self.selected_session_id = session_id
            self._load_session_log(session_id)
            self.refresh_artifacts()
            self._update_session_detail()
            self.query_one("#command", Input).focus()

        self.push_screen(SessionPalette(self._session_rows()), selected)

    def _start_task(self, task: str) -> None:
        session = self.manager.start(task)
        self.selected_session_id = session.id
        self.selected_artifact_path = None
        self._load_session_log(session.id)
        self.refresh_sessions()
        self.refresh_artifacts()

    def _session_rows(self) -> list[tuple[str, str, str, str, str]]:
        rows = []
        for session in self.store.list():
            rows.append(
                (
                    session.id,
                    session.status,
                    _format_age(session.updated_ms / 1000),
                    _dataset_run_label(session.cwd),
                    self._task_for_session(session),
                )
            )
        return rows

    def _open_artifact(self, path: Optional[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        if not path:
            log.write("[#e06c75]no selected artifact[/]")
            return
        artifact = Path(path).expanduser()
        if not artifact.exists():
            log.write(f"[#e06c75]artifact not found: {escape(str(artifact))}[/]")
            return
        try:
            subprocess.Popen(["open", str(artifact)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            log.write(f"[#7fd88f]opened {escape(str(artifact))}[/]")
        except Exception as exc:
            log.write(f"[#f5a742]open failed: {escape(str(exc))}; path: {escape(str(artifact))}[/]")

    def _write_trace(self, session_id: Optional[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        if not session_id:
            log.write("[#e06c75]no selected session for trace[/]")
            return
        from llm_browser.session.trace import write_trace_bundle

        try:
            path = write_trace_bundle(self.store, session_id)
        except Exception as exc:
            log.write(f"[#e06c75]trace failed: {escape(str(exc))}[/]")
            return
        self.selected_artifact_path = str(path)
        log.write(f"[#7fd88f]trace written: {escape(str(path))}[/]")
        self.refresh_artifacts()

    def _start_self_eval(self, session_id: Optional[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        if not session_id:
            log.write("[#e06c75]no selected session for eval[/]")
            return
        from llm_browser.session.trace import build_self_eval_prompt

        try:
            prompt = build_self_eval_prompt(self.store, session_id)
        except Exception as exc:
            log.write(f"[#e06c75]eval prompt failed: {escape(str(exc))}[/]")
            return
        child = self.manager.start(prompt, parent_id=session_id)
        self.selected_session_id = child.id
        self._load_session_log(child.id)

    def _write_dataset_report(self, run_id_or_path: str) -> None:
        log = self.query_one("#transcript", RichLog)
        try:
            manifest = load_manifest(self.store.state_dir, run_id_or_path)
            summary = summarize_manifest(manifest)
        except Exception as exc:
            log.write(f"[#e06c75]report failed: {escape(str(exc))}[/]")
            return

        failed = _short_task_list(summary["failed_task_ids"])
        pending = _short_task_list(summary["pending_task_ids"])
        log.write(
            "[bold #eeeeee]dataset report[/bold #eeeeee] "
            f"{escape(str(summary['run_id']))}  "
            f"{escape(str(summary['dataset']))}  "
            f"passed [#7fd88f]{summary['passed']}[/] / {summary['selected']}  "
            f"failed [#e06c75]{summary['failed']}[/]  "
            f"pending [#f5a742]{summary['pending']}[/]"
        )
        log.write(f"[#e06c75]failed:[/] {escape(failed)}")
        log.write(f"[#f5a742]pending:[/] {escape(pending)}")

    def _update_statusbar(self) -> None:
        sessions = self.store.list()
        counts: dict[str, int] = {}
        for session in sessions:
            counts[session.status] = counts.get(session.status, 0) + 1
        meta = (
            f"[#5c9cf5]Build[/] [#808080]·[/] "
            f"[#eeeeee]{escape(self.model_label or '-')}[/] "
            f"[#808080]{escape(self.provider_label)}  {escape(_browser_runtime_label())}[/]"
        )
        run_summary = self._selected_run_summary_text()
        if run_summary:
            meta += f"  {run_summary}"
        self.query_one("#composer-meta", Static).update(meta)

        selected_running = False
        if self.selected_session_id:
            selected = self.store.load(self.selected_session_id)
            selected_running = bool(selected and selected.status in {"created", "running"})
        left = "esc interrupt" if selected_running else "tab sessions"
        right = (
            f"{len(sessions)} sessions  "
            f"[#5c9cf5]{counts.get('running', 0)} running[/]  "
            f"[#7fd88f]{counts.get('done', 0)} done[/]  "
            f"[#e06c75]{counts.get('failed', 0)} failed[/]  "
            "ctrl+p commands"
        )
        self.query_one("#hintbar", Static).update(f"[#eeeeee]{left}[/]  [#808080]{right}[/]")

        cwd = "-"
        if self.selected_session_id:
            session = self.store.load(self.selected_session_id)
            if session is not None:
                cwd = _compact_path(session.cwd)
        self.query_one("#sidebar-footer", Static).update(
            f"[#808080]{escape(cwd)}[/]\n[#7fd88f]•[/] [bold #808080]{PRODUCT_NAME}[/bold #808080]"
        )

    def _update_session_detail(self) -> None:
        detail = self.query_one("#session-detail", Static)
        session_id = self.selected_session_id
        if not session_id:
            detail.update("No session selected.")
            return
        session = self.store.load(session_id)
        if session is None:
            detail.update(f"Missing session: {escape(session_id)}")
            return
        events = self.store.events.read(session.id)
        images = sum(1 for event in events if event.type == "tool.image")
        tools = sum(1 for event in events if event.type == "tool.started")
        artifacts = len(_artifact_paths(session))
        task = self._task_for_session(session)
        current_tool = _current_tool(events)
        final_line = _final_line(events)
        run_id = _dataset_run_id_from_path(session.cwd)
        run_line = self._dataset_run_detail(run_id) if run_id else "-"
        latest_image = _latest_image_line(events)
        title = task[:36] if task else "Browser session"
        detail.update(
            f"[bold #eeeeee]{escape(title)}[/bold #eeeeee]\n\n"
            f"[bold #eeeeee]Context[/bold #eeeeee]\n"
            f"[#808080]{len(events):,} events[/]\n"
            f"[#808080]{tools} tools[/]\n"
            f"[#808080]{images} images[/]\n"
            f"[#808080]{artifacts} artifacts[/]\n\n"
            f"[bold #eeeeee]Session[/bold #eeeeee]\n"
            f"{_status_markup(session.status)}  [#808080]{escape(session.id)}[/]\n"
            f"[#808080]parent {escape(session.parent_id or '-')}[/]\n"
            f"[#808080]updated {_format_age(session.updated_ms / 1000)}[/]\n\n"
            f"[bold #eeeeee]Current[/bold #eeeeee]\n"
            f"[#808080]{escape(current_tool)}[/]\n"
            f"[#808080]{escape(latest_image)}[/]\n"
            f"[#808080]{escape(final_line[:80])}[/]\n"
            f"[#808080]{escape(run_line[:80])}[/]"
        )

    def _preview_artifact(self, path: Optional[str], force: bool = False) -> None:
        preview = self.query_one("#artifact-preview", RichLog)
        if not path:
            self._preview_key = None
            preview.clear()
            preview.write("[#808080]No artifact selected.[/]")
            return
        artifact = Path(path)
        if not artifact.exists():
            self._preview_key = None
            preview.clear()
            preview.write(f"[#e06c75]Missing artifact: {escape(str(artifact))}[/]")
            return
        stat = artifact.stat()
        key = (str(artifact), stat.st_mtime, stat.st_size)
        if not force and key == self._preview_key:
            return
        self._preview_key = key
        preview.clear()
        kind = _artifact_kind(artifact)
        preview.write(f"[bold #eeeeee]{escape(artifact.name)}[/bold #eeeeee]  [#808080]{kind}  {_format_bytes(stat.st_size)}[/]")
        preview.write(f"[#808080]{escape(str(artifact))}[/]")
        if kind == "image":
            dims = _image_dimensions(artifact)
            if dims:
                preview.write(f"dimensions: {dims[0]} x {dims[1]}")
            preview.write("[#808080]Image artifact. Press `o` or `/open` to view it.[/]")
            meta = artifact.with_suffix(".json")
            if meta.exists():
                try:
                    preview.write(escape(meta.read_text(encoding="utf-8")[:1200]))
                except Exception:
                    pass
            return
        if artifact.suffix.lower() in {".txt", ".json", ".jsonl", ".md", ".html", ".csv", ".tsv", ".py"}:
            try:
                preview.write(escape(artifact.read_text(encoding="utf-8", errors="replace")[:4000]))
            except Exception as exc:
                preview.write(f"[#f5a742]preview failed: {escape(str(exc))}[/]")
            return
        preview.write("[#808080]Binary artifact. Press `o` or `/open` to view it.[/]")

    def _task_for_session(self, session: SessionMetadata) -> str:
        for event in self.store.events.read(session.id):
            if event.type == "session.input":
                return _summarize_task_text(str(event.payload.get("text") or ""))
        return ""

    def _selected_dataset_run_id(self) -> Optional[str]:
        if not self.selected_session_id:
            return None
        session = self.store.load(self.selected_session_id)
        if session is None:
            return None
        return _dataset_run_id_from_path(session.cwd)

    def _selected_run_summary_text(self) -> str:
        run_id = self._selected_dataset_run_id()
        if not run_id:
            return ""
        try:
            summary = summarize_manifest(load_manifest(self.store.state_dir, run_id))
        except Exception:
            return f"[#808080]run[/] {escape(run_id)}"
        return (
            f"[#808080]run[/] {escape(run_id)} "
            f"{_progress_bar(summary['passed'], summary['selected'], width=10)} "
            f"[#7fd88f]{summary['passed']}[/]/[bold]{summary['selected']}[/bold] "
            f"[#f5a742]{summary['pending']} pending[/]"
        )

    def _dataset_run_detail(self, run_id: str) -> str:
        try:
            summary = summarize_manifest(load_manifest(self.store.state_dir, run_id))
        except Exception:
            return run_id
        return (
            f"{run_id} {_progress_bar(summary['passed'], summary['selected'], width=16)} "
            f"{summary['passed']}/{summary['selected']} passed, "
            f"{summary['failed']} failed, {summary['pending']} pending"
        )


def _artifact_paths(session: SessionMetadata) -> list[Path]:
    paths: list[Path] = []
    if not session.artifact_dir.exists():
        artifact_paths: list[Path] = []
    else:
        artifact_paths = []
        for path in session.artifact_dir.rglob("*"):
            if not path.is_file():
                continue
            parts = path.relative_to(session.artifact_dir).parts
            if "chrome-profile" in parts or "__pycache__" in parts:
                continue
            artifact_paths.append(path)
    paths.extend(artifact_paths)
    state_dir = session.state_dir.resolve()
    cwd = session.cwd.resolve()
    if cwd.exists() and cwd != session.artifact_dir.resolve() and state_dir in cwd.parents:
        paths.extend([path for path in cwd.rglob("*") if path.is_file()])
    return sorted(set(paths), key=lambda path: path.stat().st_mtime, reverse=True)[:200]


def _browser_headless_default() -> bool:
    value = os.environ.get("LLM_BROWSER_HEADLESS")
    if value is None:
        return True
    return value.lower() in {"1", "true", "yes", "on"}


def _browser_runtime_label() -> str:
    diagnostics = browser_runtime_diagnostics()
    mode = str(diagnostics.get("mode") or "auto")
    if mode == "chromium" and diagnostics.get("headless_env"):
        return "chromium headless"
    if mode == "cloud":
        cloud = diagnostics.get("cloud") or {}
        profile = cloud.get("profile_name") or cloud.get("profile_id")
        return f"cloud {profile}" if profile else "cloud"
    if mode == "real":
        ports = ((diagnostics.get("real_chrome") or {}).get("active_profile_ports") or [])
        return f"real chrome :{ports[0].get('port')}" if ports else "real chrome"
    if mode == "cdp":
        return "cdp"
    return mode


def _browser_runtime_detail() -> str:
    diagnostics = browser_runtime_diagnostics()
    real = diagnostics.get("real_chrome") or {}
    cloud = diagnostics.get("cloud") or {}
    ports = ", ".join(str(item.get("port")) for item in real.get("active_profile_ports") or []) or "-"
    parts = [
        "browser config",
        f"mode: {diagnostics.get('mode')}",
        f"headless env: {diagnostics.get('headless_env')}",
        f"cdp http: {diagnostics.get('cdp_http_url') or '-'}",
        f"cdp ws: {diagnostics.get('cdp_ws_url') or '-'}",
        f"real chrome ports: {ports}",
        f"cloud key: {'set' if cloud.get('api_key_available') else 'missing'}",
        f"cloud profile: {cloud.get('profile_name') or cloud.get('profile_id') or '-'}",
    ]
    return "\n".join(parts)


def _format_event_for_transcript(event: Event) -> str:
    payload = event.payload
    if event.type == "session.created":
        return f"session {event.session_id} created"
    if event.type == "session.input":
        return _summarize_task_text(str(payload.get("text") or ""))
    if event.type == "session.status":
        return f"status {payload.get('status', '')}"
    if event.type == "session.cancel_requested":
        return f"cancel requested: {payload.get('reason', '')}"
    if event.type == "session.compacted":
        return f"compacted {payload.get('before_messages')} -> {payload.get('after_messages')} messages"
    if event.type == "session.deadline_warning":
        return f"deadline warning: {payload.get('remaining_s')}s remaining"
    if event.type == "tool.started":
        return f"→ {payload.get('name') or 'tool'} {payload.get('tool_call_id') or ''}".strip()
    if event.type == "tool.image":
        image = payload.get("image") or {}
        path = Path(str(image.get("path") or ""))
        label = image.get("label") or "image"
        return f"image: {label} -> {path.name or path}"
    if event.type == "tool.output":
        text = _compact_inline(payload.get("text", ""))
        return f"  {payload.get('name') or 'tool'} {payload.get('stream') or ''} {text}".strip()
    if event.type == "tool.finished":
        output = payload.get("output") or {}
        text = _compact_inline(output.get("text", ""))
        suffix = f" {text}" if text else ""
        return f"✓ {payload.get('name') or 'tool'}{suffix}"
    if event.type == "tool.failed":
        return f"tool failed: {payload.get('name') or 'tool'} {payload.get('error') or ''}".strip()
    if event.type == "session.done":
        text = _compact_inline(payload.get("result", ""), limit=220)
        return f"done: {text}" if text else "done"
    if event.type == "session.failed":
        return f"failed: {payload.get('error') or ''}".strip()
    return f"{event.type}: {payload}"


def _format_events_for_transcript(events: list[Event]) -> list[tuple[str, str]]:
    lines: list[tuple[str, str]] = []
    model_buffers: dict[str, str] = {}

    def flush(session_id: str) -> None:
        text = model_buffers.pop(session_id, "")
        if text.strip():
            lines.append((" ".join(text.strip().split()), "model.delta"))

    for event in events:
        if event.type == "model.delta":
            text = str(event.payload.get("text") or "")
            if not text:
                continue
            buffered = model_buffers.get(event.session_id, "") + text
            model_buffers[event.session_id] = buffered
            if "\n" in buffered or len(buffered) >= 700:
                flush(event.session_id)
            continue
        flush(event.session_id)
        lines.append((_format_event_for_transcript(event), event.type))

    for session_id in list(model_buffers):
        flush(session_id)
    return lines


def _format_events_for_log(events: list[Event]) -> list[tuple[str, str]]:
    lines: list[tuple[str, str]] = []
    model_buffers: dict[str, str] = {}

    def flush(session_id: str) -> None:
        text = model_buffers.pop(session_id, "")
        if text.strip():
            collapsed = " ".join(text.strip().split())
            lines.append((f"[{session_id}] model: {collapsed}", "model.delta"))

    for event in events:
        if event.type == "model.delta":
            text = str(event.payload.get("text") or "")
            if not text:
                continue
            buffered = model_buffers.get(event.session_id, "") + text
            model_buffers[event.session_id] = buffered
            if "\n" in buffered or len(buffered) >= 700:
                flush(event.session_id)
            continue
        flush(event.session_id)
        lines.append((format_event(event), event.type))

    for session_id in list(model_buffers):
        flush(session_id)
    return lines


def _summarize_task_text(text: str) -> str:
    task = text
    marker = "\nTask:\n"
    if marker in task:
        task = task.split(marker, 1)[1]
    for stop in ("\n\nRuntime budget:", "\nRuntime budget:"):
        if stop in task:
            task = task.split(stop, 1)[0]
    return " ".join(task.split())


def _compact_inline(value: object, limit: int = 160) -> str:
    text = " ".join(str(value or "").strip().split())
    if len(text) > limit:
        return text[: max(0, limit - 3)] + "..."
    return text


def _compact_path(path: Path, limit: int = 38) -> str:
    text = str(path.expanduser())
    home = str(Path.home())
    if text.startswith(home):
        text = "~" + text[len(home) :]
    if len(text) > limit:
        return "…" + text[-(limit - 1) :]
    return text


def _dataset_run_id_from_path(path: Path) -> Optional[str]:
    parts = path.parts
    for index, part in enumerate(parts):
        if part == "dataset-runs" and index + 1 < len(parts):
            return parts[index + 1]
    return None


def _dataset_task_id_from_path(path: Path) -> Optional[str]:
    name = path.name
    match = re.match(r"task-(.+)-workspace$", name)
    if match:
        return match.group(1)
    return None


def _dataset_run_label(path: Path) -> str:
    run_id = _dataset_run_id_from_path(path)
    if not run_id:
        return "-"
    task_id = _dataset_task_id_from_path(path)
    compact_run = run_id
    if compact_run.startswith("real-v8-"):
        compact_run = "v8-" + compact_run.removeprefix("real-v8-")
    if compact_run.startswith("real-v14-"):
        compact_run = "v14-" + compact_run.removeprefix("real-v14-")
    label = compact_run[:18]
    return f"{label}:{task_id}" if task_id else label


def _progress_bar(done: int, total: int, width: int = 12) -> str:
    width = max(4, width)
    if total <= 0:
        filled = 0
    else:
        filled = min(width, max(0, round((done / total) * width)))
    empty = width - filled
    return "[#5c9cf5]" + ("█" * filled) + "[/][#323232]" + ("░" * empty) + "[/]"


def _latest_image_line(events: list[Event]) -> str:
    for event in reversed(events):
        if event.type != "tool.image":
            continue
        image = event.payload.get("image") or {}
        label = str(image.get("label") or "image")
        path = Path(str(image.get("path") or ""))
        name = path.name if str(path) else "-"
        return f"{label} -> {name}"
    return "-"


def _short_task_list(task_ids: list[str], limit: int = 12) -> str:
    if not task_ids:
        return "-"
    rendered = ", ".join(str(task_id) for task_id in task_ids[:limit])
    if len(task_ids) > limit:
        rendered += f" +{len(task_ids) - limit}"
    return rendered


def _status_markup(status: str) -> str:
    styles = {
        "running": "bold #5c9cf5",
        "done": "bold #7fd88f",
        "failed": "bold #e06c75",
        "cancelled": "bold #f5a742",
        "created": "#808080",
    }
    return f"[{styles.get(status, '#eeeeee')}]{escape(status)}[/]"


def _status_text(status: str) -> Text:
    styles = {
        "running": "bold #5c9cf5",
        "done": "bold #7fd88f",
        "failed": "bold #e06c75",
        "cancelled": "bold #f5a742",
        "created": "#808080",
    }
    label = status[:9]
    return Text(label, style=styles.get(status, "#eeeeee"))


def _current_tool(events: list[Event]) -> str:
    started: dict[str, str] = {}
    finished: set[str] = set()
    for event in events:
        if event.type == "tool.started":
            call_id = str(event.payload.get("tool_call_id") or "")
            started[call_id] = str(event.payload.get("name") or "tool")
        elif event.type in {"tool.finished", "tool.failed"}:
            finished.add(str(event.payload.get("tool_call_id") or ""))

    for call_id, name in reversed(list(started.items())):
        if call_id not in finished:
            return f"{name} {call_id}".strip()

    for event in reversed(events):
        if event.type == "tool.finished":
            return f"{event.payload.get('name') or 'tool'} done"
        if event.type == "tool.failed":
            return f"{event.payload.get('name') or 'tool'} failed"
    return "-"


def _final_line(events: list[Event]) -> str:
    for event in reversed(events):
        if event.type == "session.done":
            return str(event.payload.get("result") or "done")
        if event.type == "session.failed":
            return str(event.payload.get("error") or "failed")
        if event.type == "session.cancelled":
            return str(event.payload.get("reason") or "cancelled")
        if event.type == "tool.finished":
            output = event.payload.get("output") or {}
            text = str(output.get("text") or "").strip()
            return text or f"{event.payload.get('name') or 'tool'} finished"
        if event.type == "tool.failed":
            output = event.payload.get("output") or {}
            text = str(output.get("text") or "").strip()
            return text or f"{event.payload.get('name') or 'tool'} failed"
        if event.type == "tool.started":
            return f"{event.payload.get('name') or 'tool'} running"
    return "-"


def _artifact_kind(path: Path) -> str:
    if "traces" in path.parts:
        return "trace"
    if "downloads" in path.parts:
        return "download"
    suffix = path.suffix.lower()
    if suffix in {".png", ".jpg", ".jpeg", ".webp"}:
        return "image"
    if suffix in {".json", ".jsonl"}:
        return "json"
    if "tool-output" in path.parts:
        return "tool"
    if "dataset-runs" in path.parts:
        return "workspace"
    return suffix.lstrip(".") or "file"


def _image_dimensions(path: Path) -> Optional[tuple[int, int]]:
    try:
        from PIL import Image

        with Image.open(path) as image:
            return image.size
    except Exception:
        return None


def _format_bytes(size: int) -> str:
    value = float(size)
    for unit in ("B", "KB", "MB", "GB"):
        if value < 1024 or unit == "GB":
            if unit == "B":
                return f"{int(value)} {unit}"
            return f"{value:.1f} {unit}"
        value /= 1024
    return f"{size} B"


def _format_age(mtime: float) -> str:
    age = max(0, int(time.time() - mtime))
    if age < 60:
        return f"{age}s ago"
    if age < 3600:
        return f"{age // 60}m ago"
    if age < 86400:
        return f"{age // 3600}h ago"
    return f"{age // 86400}d ago"


class TextualTui:
    def __init__(
        self,
        store: SessionStore,
        provider_factory: Optional[ProviderFactory] = None,
        max_turns: int = 80,
        provider_label: str = "fake",
        model_label: Optional[str] = None,
        config: Optional[dict] = None,
    ) -> None:
        self.app = BrowserUseTerminalApp(
            store,
            provider_factory=provider_factory,
            max_turns=max_turns,
            provider_label=provider_label,
            model_label=model_label,
            config=config,
        )

    def run(self) -> int:
        self.app.run()
        return 0
