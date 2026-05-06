from __future__ import annotations

import queue
import shlex
import subprocess
import threading
import time
from pathlib import Path
from typing import Callable, Optional

from rich.markup import escape
from rich.text import Text
from textual.app import App, ComposeResult
from textual.containers import Horizontal, Vertical
from textual.widgets import DataTable, Footer, Header, Input, RichLog, Static

from llm_browser.agent import SessionManager
from llm_browser.brand import PRODUCT_NAME
from llm_browser.datasets import build_dataset_prompt, load_dataset, select_tasks
from llm_browser.events import Event
from llm_browser.provider.base import Provider
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tui.simple import format_event


ProviderFactory = Callable[[], Optional[Provider]]


class BrowserUseTerminalApp(App[None]):
    CSS = """
    Screen {
        background: #0b0d10;
        color: #e5e1d8;
    }

    Header {
        background: #171a20;
        color: #f4f0e8;
    }

    #body {
        height: 1fr;
    }

    #statusbar {
        height: 1;
        padding: 0 1;
        background: #20242b;
        color: #c9c3b8;
        text-style: bold;
    }

    #left {
        width: 44;
        min-width: 34;
        background: #11141a;
        border: tall #2b3039;
    }

    #center {
        width: 1fr;
        min-width: 44;
    }

    #right {
        width: 56;
        min-width: 36;
        background: #11141a;
        border: tall #2b3039;
    }

    #sessions-title, #events-title, #artifacts-title, #detail-title, #preview-title, #help-title {
        height: 1;
        padding: 0 1;
        background: #20242b;
        color: #9ccfd8;
        text-style: bold;
    }

    #sessions {
        height: 1fr;
        background: #11141a;
    }

    #help {
        height: 8;
        padding: 1;
        color: #b9b2a7;
        background: #0f1116;
    }

    #events {
        height: 1fr;
        border: tall #2b3039;
        background: #0b0d10;
    }

    #session-detail {
        height: 10;
        padding: 1;
        color: #d8d1c8;
        background: #0f1116;
    }

    #artifacts {
        height: 1fr;
        background: #11141a;
    }

    #artifact-preview {
        height: 12;
        border: tall #2b3039;
        background: #0b0d10;
    }

    #command {
        height: 3;
        border: tall #9ccfd8;
        background: #171a20;
        color: #f4f0e8;
    }

    DataTable {
        scrollbar-color: #3c5660;
        scrollbar-background: #0f1116;
    }
    """

    BINDINGS = [
        ("ctrl+c", "cancel_selected", "Cancel"),
        ("ctrl+r", "refresh", "Refresh"),
        ("ctrl+l", "clear_log", "Clear"),
        ("o", "open_artifact", "Open"),
        ("enter", "open_artifact", "Open"),
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
        self.selected_artifact_path: Optional[str] = None
        self._preview_key: Optional[tuple[str, float, int]] = None
        self._model_buffers: dict[str, str] = {}
        self._stop = threading.Event()
        self._listener: Optional[threading.Thread] = None

    def compose(self) -> ComposeResult:
        yield Header(name=PRODUCT_NAME, show_clock=True)
        yield Static("", id="statusbar")
        with Horizontal(id="body"):
            with Vertical(id="left"):
                yield Static("sessions", id="sessions-title")
                sessions = DataTable(id="sessions", cursor_type="row")
                sessions.add_columns("id", "state", "age", "task")
                yield sessions
                yield Static("commands", id="help-title")
                yield Static(
                    "run <task>\n"
                    "dataset <name> [count|--all]\n"
                    "dataset <name> --task-id <id>\n"
                    "show/resume/cancel [id]\n"
                    "trace/eval [id]\n"
                    "open [artifact]\n"
                    "ctrl-r refresh  ctrl-l clear",
                    id="help",
                )
            with Vertical(id="center"):
                yield Static("events", id="events-title")
                yield RichLog(id="events", wrap=True, highlight=True, markup=True)
            with Vertical(id="right"):
                yield Static("selected session", id="detail-title")
                yield Static("", id="session-detail")
                yield Static("artifacts", id="artifacts-title")
                artifacts = DataTable(id="artifacts", cursor_type="row")
                artifacts.add_columns("kind", "name", "size", "modified")
                yield artifacts
                yield Static("preview", id="preview-title")
                yield RichLog(id="artifact-preview", wrap=True, highlight=True, markup=True)
        yield Input(
            placeholder="run <task>  |  dataset <name> [count]  |  show <id>  |  trace/eval/open  |  help",
            id="command",
        )
        yield Footer()

    def on_mount(self) -> None:
        self.title = PRODUCT_NAME
        self.sub_title = "raw CDP browser agent"
        self._write_banner()
        self.refresh_sessions()
        self._update_statusbar()
        self._update_session_detail()
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

    def _write_banner(self) -> None:
        log = self.query_one("#events", RichLog)
        log.write("[bold #9ccfd8]browser use terminal[/bold #9ccfd8]")
        log.write(
            "Commands: [bold]run[/bold] a task, [bold]dataset real_v8 --task-id 20[/bold], "
            "[bold]resume[/bold] selected, [bold]cancel[/bold] a session, [bold]show[/bold] a session, "
            "[bold]open[/bold] an artifact."
        )

    def _handle_event(self, event: Event) -> None:
        if self.selected_session_id is None:
            self.selected_session_id = event.session_id

        if event.type == "model.delta":
            self._append_model_delta(event)
            return

        self._flush_model_delta(event.session_id)
        self._write_log_line(format_event(event), event.type)
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
        self._write_log_line(f"[{session_id}] model: {collapsed}", "model.delta")

    def _flush_all_model_deltas(self) -> None:
        for session_id in list(self._model_buffers):
            self._flush_model_delta(session_id)

    def _write_log_line(self, line: str, event_type: str) -> None:
        log = self.query_one("#events", RichLog)
        escaped = escape(line)
        if event_type == "tool.failed":
            log.write(f"[red]{escaped}[/red]")
        elif event_type == "tool.image":
            log.write(f"[bold #9ccfd8]{escaped}[/bold #9ccfd8]")
        elif event_type == "tool.output":
            log.write(f"[dim]{escaped}[/dim]")
        elif event_type == "model.delta":
            log.write(f"[#d8cab8]{escaped}[/]")
        elif event_type in {"session.done", "session.cancelled"}:
            log.write(f"[green]{escaped}[/green]")
        elif event_type == "session.failed":
            log.write(f"[bold red]{escaped}[/bold red]")
        elif event_type == "session.deadline_warning":
            log.write(f"[yellow]{escaped}[/yellow]")
        else:
            log.write(escaped)

    def on_input_submitted(self, event: Input.Submitted) -> None:
        line = event.value.strip()
        event.input.value = ""
        if not line:
            return
        self._handle_command(line)

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        if event.data_table.id == "sessions":
            self.selected_session_id = str(event.row_key.value)
            self._load_session_log(self.selected_session_id)
            self.refresh_artifacts()
            self._update_session_detail()
        elif event.data_table.id == "artifacts":
            self.selected_artifact_path = str(event.row_key.value)
            self._preview_artifact(self.selected_artifact_path, force=True)

    def _handle_command(self, line: str) -> None:
        log = self.query_one("#events", RichLog)
        normalized_line = line.lstrip("/")
        if normalized_line.startswith("run "):
            task = normalized_line[4:].strip()
            if not task:
                log.write("[red]run requires a task[/red]")
                return
            session = self.manager.start(task)
            self.selected_session_id = session.id
            log.write(f"[bold #9ccfd8]started {session.id}[/bold #9ccfd8] {escape(task[:160])}")
            self.refresh_sessions()
            return

        try:
            args = shlex.split(normalized_line)
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
        elif command == "sessions":
            self.refresh_sessions()
            log.write("[green]sessions refreshed[/green]")
        elif command == "artifacts":
            self.refresh_artifacts()
            log.write("[green]artifacts refreshed[/green]")
        elif command == "show" and len(args) == 2:
            self.selected_session_id = args[1]
            self._load_session_log(args[1])
            self.refresh_artifacts()
        elif command == "resume":
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            instruction = " ".join(args[2:]) if len(args) > 2 else "Continue from the previous session state."
            if not session_id:
                log.write("[red]no selected session to resume[/red]")
                return
            parent = self.store.load(session_id)
            if parent is None:
                log.write(f"[red]session not found: {escape(session_id)}[/red]")
                return
            resumed = self.manager.start(instruction, parent_id=parent.id)
            self.selected_session_id = resumed.id
            log.write(f"[bold #9ccfd8]started resume child {escape(resumed.id)} for {escape(parent.id)}[/bold #9ccfd8]")
        elif command == "cancel":
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            if not session_id:
                log.write("[red]no selected session to cancel[/red]")
                return
            self.manager.cancel(session_id)
            log.write(f"[yellow]cancel requested for {escape(session_id)}[/yellow]")
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
                        log.write(f"[red]invalid dataset option: {escape(rest[index])}[/red]")
                        return
                    index += 1
            tasks = select_tasks(load_dataset(args[1]), count=count, task_ids=task_ids or None)
            for task in tasks:
                session = self.manager.start(build_dataset_prompt(task, headless=True))
                self.selected_session_id = session.id
                log.write(
                    f"[bold #9ccfd8]started {escape(task.dataset)} task {escape(task.task_id)} "
                    f"as {escape(session.id)}[/bold #9ccfd8]"
                )
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
        for line, event_type in _format_events_for_log(self.store.events.read(session.id)[-400:]):
            self._write_log_line(line, event_type)
        self._update_session_detail()

    def refresh_sessions(self) -> None:
        table = self.query_one("#sessions", DataTable)
        table.clear()
        for session in self.store.list():
            task = self._task_for_session(session)
            table.add_row(
                session.id,
                _status_text(session.status),
                _format_age(session.updated_ms / 1000),
                task[:46],
                key=session.id,
            )
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
            return
        session = self.store.load(session_id)
        if session is None:
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
                _format_age(stat.st_mtime),
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
        self.query_one("#events", RichLog).clear()

    def action_open_artifact(self) -> None:
        self._open_artifact(self.selected_artifact_path)

    def _open_artifact(self, path: Optional[str]) -> None:
        log = self.query_one("#events", RichLog)
        if not path:
            log.write("[red]no selected artifact[/red]")
            return
        artifact = Path(path).expanduser()
        if not artifact.exists():
            log.write(f"[red]artifact not found: {escape(str(artifact))}[/red]")
            return
        try:
            subprocess.Popen(["open", str(artifact)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            log.write(f"[green]opened {escape(str(artifact))}[/green]")
        except Exception as exc:
            log.write(f"[yellow]open failed: {escape(str(exc))}; path: {escape(str(artifact))}[/yellow]")

    def _write_trace(self, session_id: Optional[str]) -> None:
        log = self.query_one("#events", RichLog)
        if not session_id:
            log.write("[red]no selected session for trace[/red]")
            return
        from llm_browser.session.trace import write_trace_bundle

        try:
            path = write_trace_bundle(self.store, session_id)
        except Exception as exc:
            log.write(f"[red]trace failed: {escape(str(exc))}[/red]")
            return
        self.selected_artifact_path = str(path)
        log.write(f"[green]trace written: {escape(str(path))}[/green]")
        self.refresh_artifacts()

    def _start_self_eval(self, session_id: Optional[str]) -> None:
        log = self.query_one("#events", RichLog)
        if not session_id:
            log.write("[red]no selected session for eval[/red]")
            return
        from llm_browser.session.trace import build_self_eval_prompt

        try:
            prompt = build_self_eval_prompt(self.store, session_id)
        except Exception as exc:
            log.write(f"[red]eval prompt failed: {escape(str(exc))}[/red]")
            return
        child = self.manager.start(prompt, parent_id=session_id)
        self.selected_session_id = child.id
        log.write(f"[bold #9ccfd8]started self-eval {escape(child.id)} for {escape(session_id)}[/bold #9ccfd8]")

    def _update_statusbar(self) -> None:
        sessions = self.store.list()
        counts: dict[str, int] = {}
        for session in sessions:
            counts[session.status] = counts.get(session.status, 0) + 1
        text = (
            f"[bold #f4f0e8]{PRODUCT_NAME}[/bold #f4f0e8]  "
            f"[#b9b2a7]sessions[/] [bold]{len(sessions)}[/bold]  "
            f"[#9ccfd8]running[/] [bold]{counts.get('running', 0)}[/bold]  "
            f"[green]done[/green] [bold]{counts.get('done', 0)}[/bold]  "
            f"[red]failed[/red] [bold]{counts.get('failed', 0)}[/bold]  "
            f"[#b9b2a7]selected[/] {escape(self.selected_session_id or '-')}"
        )
        self.query_one("#statusbar", Static).update(text)

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
        detail.update(
            f"[bold]{escape(session.id)}[/bold]\n"
            f"status: {escape(session.status)}\n"
            f"parent: {escape(session.parent_id or '-')}\n"
            f"events: {len(events)}  tools: {tools}  images: {images}  artifacts: {artifacts}\n"
            f"updated: {_format_age(session.updated_ms / 1000)}\n"
            f"cwd: {escape(str(session.cwd))}\n"
            f"task: {escape(task[:220])}"
        )

    def _preview_artifact(self, path: Optional[str], force: bool = False) -> None:
        preview = self.query_one("#artifact-preview", RichLog)
        if not path:
            self._preview_key = None
            preview.clear()
            preview.write("[dim]No artifact selected.[/dim]")
            return
        artifact = Path(path)
        if not artifact.exists():
            self._preview_key = None
            preview.clear()
            preview.write(f"[red]Missing artifact: {escape(str(artifact))}[/red]")
            return
        stat = artifact.stat()
        key = (str(artifact), stat.st_mtime, stat.st_size)
        if not force and key == self._preview_key:
            return
        self._preview_key = key
        preview.clear()
        kind = _artifact_kind(artifact)
        preview.write(f"[bold #9edbe8]{escape(artifact.name)}[/bold #9edbe8]  {kind}  {_format_bytes(stat.st_size)}")
        preview.write(escape(str(artifact)))
        if kind == "image":
            dims = _image_dimensions(artifact)
            if dims:
                preview.write(f"dimensions: {dims[0]} x {dims[1]}")
            preview.write("[dim]Image artifact. Press enter or `open` to view it.[/dim]")
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
                preview.write(f"[yellow]preview failed: {escape(str(exc))}[/yellow]")
            return
        preview.write("[dim]Binary artifact. Press enter or `open` to view it.[/dim]")

    def _task_for_session(self, session: SessionMetadata) -> str:
        for event in self.store.events.read(session.id):
            if event.type == "session.input":
                return _summarize_task_text(str(event.payload.get("text") or ""))
        return ""


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


def _status_text(status: str) -> Text:
    styles = {
        "running": "bold #9ccfd8",
        "done": "bold green",
        "failed": "bold red",
        "cancelled": "bold yellow",
        "created": "#b9b2a7",
    }
    label = status.upper()[:9]
    return Text(label, style=styles.get(status, "#d8cab8"))


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
    ) -> None:
        self.app = BrowserUseTerminalApp(store, provider_factory=provider_factory, max_turns=max_turns)

    def run(self) -> int:
        self.app.run()
        return 0
