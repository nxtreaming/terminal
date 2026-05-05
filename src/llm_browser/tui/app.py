from __future__ import annotations

import queue
import shlex
import threading
from pathlib import Path
from typing import Callable, Optional

from rich.markup import escape
from textual.app import App, ComposeResult
from textual.containers import Horizontal, Vertical
from textual.widgets import DataTable, Footer, Header, Input, RichLog, Static

from llm_browser.agent import SessionManager
from llm_browser.brand import PRODUCT_NAME
from llm_browser.events import Event
from llm_browser.provider.base import Provider
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tui.simple import format_event


ProviderFactory = Callable[[], Optional[Provider]]


class BrowserUseTerminalApp(App[None]):
    CSS = """
    Screen {
        background: #0b0f14;
        color: #e6edf3;
    }

    Header {
        background: #111827;
        color: #f8fafc;
    }

    #body {
        height: 1fr;
    }

    #left {
        width: 34;
        min-width: 28;
        background: #0f1720;
        border: tall #263241;
    }

    #main {
        width: 1fr;
    }

    #sessions-title, #events-title, #artifacts-title {
        height: 1;
        padding: 0 1;
        background: #17202b;
        color: #9ccfd8;
        text-style: bold;
    }

    #sessions {
        height: 1fr;
        background: #0f1720;
    }

    #events {
        height: 2fr;
        border: tall #263241;
        background: #0b0f14;
    }

    #artifacts {
        height: 1fr;
        border: tall #263241;
        background: #0f1720;
    }

    #command {
        height: 3;
        border: tall #9ccfd8;
        background: #111827;
    }
    """

    BINDINGS = [
        ("ctrl+c", "cancel_selected", "Cancel"),
        ("ctrl+r", "refresh", "Refresh"),
        ("ctrl+l", "clear_log", "Clear"),
        ("q", "quit", "Quit"),
    ]

    def __init__(
        self,
        store: SessionStore,
        provider_factory: Optional[ProviderFactory] = None,
        max_turns: int = 80,
    ) -> None:
        super().__init__()
        self.store = store
        self.manager = SessionManager(store, provider_factory=provider_factory, max_turns=max_turns)
        self.selected_session_id: Optional[str] = None
        self._stop = threading.Event()
        self._listener: Optional[threading.Thread] = None

    def compose(self) -> ComposeResult:
        yield Header(name=PRODUCT_NAME, show_clock=True)
        with Horizontal(id="body"):
            with Vertical(id="left"):
                yield Static("sessions", id="sessions-title")
                sessions = DataTable(id="sessions", cursor_type="row")
                sessions.add_columns("id", "status", "task")
                yield sessions
            with Vertical(id="main"):
                yield Static("events", id="events-title")
                yield RichLog(id="events", wrap=True, highlight=True, markup=True)
                yield Static("artifacts", id="artifacts-title")
                artifacts = DataTable(id="artifacts", cursor_type="row")
                artifacts.add_columns("kind", "name", "path")
                yield artifacts
        yield Input(
            placeholder="run <task>  |  show <session-id>  |  cancel [session-id]  |  refresh  |  help",
            id="command",
        )
        yield Footer()

    def on_mount(self) -> None:
        self.title = PRODUCT_NAME
        self.sub_title = "raw CDP browser agent"
        self._write_banner()
        self.refresh_sessions()
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

    def _write_banner(self) -> None:
        log = self.query_one("#events", RichLog)
        log.write("[bold #9ccfd8]browser use terminal[/bold #9ccfd8]")
        log.write("Commands: [bold]run[/bold] a task, [bold]cancel[/bold] a session, [bold]show[/bold] a session.")

    def _handle_event(self, event: Event) -> None:
        if self.selected_session_id is None:
            self.selected_session_id = event.session_id
        log = self.query_one("#events", RichLog)
        line = escape(format_event(event))
        if event.type == "tool.failed":
            log.write(f"[red]{line}[/red]")
        elif event.type == "tool.image":
            log.write(f"[bold #9ccfd8]{line}[/bold #9ccfd8]")
        elif event.type in {"session.done", "session.cancelled"}:
            log.write(f"[green]{line}[/green]")
        elif event.type == "session.failed":
            log.write(f"[bold red]{line}[/bold red]")
        else:
            log.write(line)
        self.refresh_sessions()
        self.refresh_artifacts()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        line = event.value.strip()
        event.input.value = ""
        if not line:
            return
        self._handle_command(line)

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        if event.data_table.id != "sessions":
            return
        self.selected_session_id = str(event.row_key.value)
        self._load_session_log(self.selected_session_id)
        self.refresh_artifacts()

    def _handle_command(self, line: str) -> None:
        log = self.query_one("#events", RichLog)
        if line.startswith("run "):
            task = line[4:].strip()
            if not task:
                log.write("[red]run requires a task[/red]")
                return
            session = self.manager.start(task)
            self.selected_session_id = session.id
            log.write(f"[bold #9ccfd8]started {session.id}[/bold #9ccfd8] {escape(task[:160])}")
            self.refresh_sessions()
            return

        try:
            args = shlex.split(line)
        except ValueError as exc:
            log.write(f"[red]parse error: {escape(str(exc))}[/red]")
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
        elif command == "show" and len(args) == 2:
            self.selected_session_id = args[1]
            self._load_session_log(args[1])
            self.refresh_artifacts()
        elif command == "cancel":
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            if not session_id:
                log.write("[red]no selected session to cancel[/red]")
                return
            self.manager.cancel(session_id)
            log.write(f"[yellow]cancel requested for {escape(session_id)}[/yellow]")
        else:
            log.write(f"[red]unknown command: {escape(command)}[/red]")

    def _load_session_log(self, session_id: str) -> None:
        log = self.query_one("#events", RichLog)
        log.clear()
        session = self.store.load(session_id)
        if session is None:
            log.write(f"[red]session not found: {escape(session_id)}[/red]")
            return
        log.write(f"[bold #9ccfd8]session {escape(session.id)}[/bold #9ccfd8] {escape(session.status)}")
        for event in self.store.events.read(session.id)[-400:]:
            log.write(escape(format_event(event)))

    def refresh_sessions(self) -> None:
        table = self.query_one("#sessions", DataTable)
        table.clear()
        for session in self.store.list():
            task = self._task_for_session(session)
            table.add_row(session.id, session.status, task[:38], key=session.id)
        if self.selected_session_id is None:
            sessions = self.store.list()
            if sessions:
                self.selected_session_id = sessions[0].id

    def refresh_artifacts(self) -> None:
        table = self.query_one("#artifacts", DataTable)
        table.clear()
        session_id = self.selected_session_id
        if not session_id:
            return
        session = self.store.load(session_id)
        if session is None:
            return
        for path in _artifact_paths(session):
            table.add_row(_artifact_kind(path), path.name, str(path), key=str(path))

    def action_cancel_selected(self) -> None:
        if self.selected_session_id:
            self.manager.cancel(self.selected_session_id)

    def action_refresh(self) -> None:
        self.refresh_sessions()
        self.refresh_artifacts()

    def action_clear_log(self) -> None:
        self.query_one("#events", RichLog).clear()

    def _task_for_session(self, session: SessionMetadata) -> str:
        for event in self.store.events.read(session.id):
            if event.type == "session.input":
                return str(event.payload.get("text") or "")
        return ""


def _artifact_paths(session: SessionMetadata) -> list[Path]:
    if not session.artifact_dir.exists():
        return []
    paths = [path for path in session.artifact_dir.rglob("*") if path.is_file()]
    return sorted(paths, key=lambda path: path.stat().st_mtime, reverse=True)[:200]


def _artifact_kind(path: Path) -> str:
    suffix = path.suffix.lower()
    if suffix in {".png", ".jpg", ".jpeg", ".webp"}:
        return "image"
    if suffix in {".json", ".jsonl"}:
        return "json"
    if "downloads" in path.parts:
        return "download"
    if "tool-output" in path.parts:
        return "tool"
    return suffix.lstrip(".") or "file"


class TextualTui:
    def __init__(
        self,
        store: SessionStore,
        provider_factory: Optional[ProviderFactory] = None,
        max_turns: int = 80,
    ) -> None:
        self.app = BrowserUseTerminalApp(store, provider_factory=provider_factory, max_turns=max_turns)

    def run(self) -> int:
        self.app.run()
        return 0
