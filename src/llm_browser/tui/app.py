from __future__ import annotations

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
from textual.containers import Horizontal, Vertical
from textual.widgets import DataTable, Footer, Header, Input, RichLog, Static

from llm_browser.agent import SessionManager
from llm_browser.brand import PRODUCT_NAME
from llm_browser.datasets import build_dataset_prompt, load_dataset, load_manifest, select_tasks, summarize_manifest
from llm_browser.events import Event
from llm_browser.provider.base import Provider
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tui.simple import format_event


ProviderFactory = Callable[[], Optional[Provider]]


class BrowserUseTerminalApp(App[None]):
    CSS = """
    Screen {
        background: #0c0e12;
        color: #eee8dc;
    }

    Header {
        background: #171b20;
        color: #f6efe3;
        text-style: bold;
    }

    #body {
        height: 1fr;
    }

    #statusbar {
        height: 1;
        padding: 0 1;
        background: #252018;
        color: #d9cdbd;
        text-style: bold;
    }

    #left {
        width: 44;
        min-width: 34;
        background: #101318;
        border: tall #33424b;
    }

    #center {
        width: 1fr;
        min-width: 44;
    }

    #right {
        width: 56;
        min-width: 36;
        background: #101318;
        border: tall #33424b;
    }

    #sessions-title, #events-title, #artifacts-title, #detail-title, #preview-title, #help-title {
        height: 1;
        padding: 0 1;
        background: #1b2325;
        color: #a6e3d7;
        text-style: bold;
    }

    #sessions {
        height: 1fr;
        background: #101318;
    }

    #help {
        height: 8;
        padding: 1;
        color: #c7baaa;
        background: #11151a;
    }

    #events {
        height: 1fr;
        border: tall #2a343a;
        background: #0c0e12;
    }

    #session-detail {
        height: 12;
        padding: 1;
        color: #ded4c8;
        background: #11151a;
    }

    #artifacts {
        height: 1fr;
        background: #101318;
    }

    #artifact-preview {
        height: 12;
        border: tall #2a343a;
        background: #0c0e12;
    }

    #command {
        height: 3;
        border: tall #a6e3d7;
        background: #171b20;
        color: #f6efe3;
    }

    DataTable {
        background: #101318;
        color: #e8ded1;
        scrollbar-color: #47656a;
        scrollbar-background: #11151a;
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
                sessions.add_columns("id", "state", "age", "run", "task")
                yield sessions
                yield Static("commands", id="help-title")
                yield Static(
                    "run <task>\n"
                    "dataset <name> [count|--all]\n"
                    "dataset <name> --task-id <id>\n"
                    "report <run-id>\n"
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
            placeholder="run <task>  |  dataset <name> [count]  |  report <run-id>  |  show/trace/eval/open  |  help",
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
        log.write("[bold #a6e3d7]browser use terminal[/bold #a6e3d7]")
        log.write(
            "Commands: [bold]run[/bold] a task, [bold]dataset real_v8 --task-id 20[/bold], "
            "[bold]resume[/bold] selected, [bold]cancel[/bold] a session, [bold]show[/bold] a session, "
            "[bold]report[/bold] a dataset run, [bold]open[/bold] an artifact."
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
        elif command == "report":
            run_id = args[1] if len(args) >= 2 else self._selected_dataset_run_id()
            if not run_id:
                log.write("[red]report requires a run id or a selected dataset session[/red]")
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
            run_label = _dataset_run_label(session.cwd)
            table.add_row(
                session.id,
                _status_text(session.status),
                _format_age(session.updated_ms / 1000),
                run_label,
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

    def _write_dataset_report(self, run_id_or_path: str) -> None:
        log = self.query_one("#events", RichLog)
        try:
            manifest = load_manifest(self.store.state_dir, run_id_or_path)
            summary = summarize_manifest(manifest)
        except Exception as exc:
            log.write(f"[red]report failed: {escape(str(exc))}[/red]")
            return

        failed = _short_task_list(summary["failed_task_ids"])
        pending = _short_task_list(summary["pending_task_ids"])
        log.write(
            "[bold #a6e3d7]dataset report[/bold #a6e3d7] "
            f"{escape(str(summary['run_id']))}  "
            f"{escape(str(summary['dataset']))}  "
            f"passed [green]{summary['passed']}[/green] / {summary['selected']}  "
            f"failed [red]{summary['failed']}[/red]  "
            f"pending [yellow]{summary['pending']}[/yellow]"
        )
        log.write(f"[red]failed:[/red] {escape(failed)}")
        log.write(f"[yellow]pending:[/yellow] {escape(pending)}")

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
            f"{self._selected_run_summary_text()} "
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
        current_tool = _current_tool(events)
        final_line = _final_line(events)
        run_id = _dataset_run_id_from_path(session.cwd)
        run_line = self._dataset_run_detail(run_id) if run_id else "-"
        latest_image = _latest_image_line(events)
        detail.update(
            f"[bold]{escape(session.id)}[/bold]\n"
            f"status: {_status_markup(session.status)}\n"
            f"parent: {escape(session.parent_id or '-')}\n"
            f"dataset: {escape(run_line)}\n"
            f"events: {len(events)}  tools: {tools}  images: {images}  artifacts: {artifacts}\n"
            f"tool: {escape(current_tool)}\n"
            f"image: {escape(latest_image)}\n"
            f"updated: {_format_age(session.updated_ms / 1000)}\n"
            f"cwd: {escape(str(session.cwd))}\n"
            f"task: {escape(task[:180])}\n"
            f"last: {escape(final_line[:180])}"
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
            return f"[#b9b2a7]run[/] {escape(run_id)}"
        return (
            f"[#b9b2a7]run[/] {escape(run_id)} "
            f"{_progress_bar(summary['passed'], summary['selected'], width=10)} "
            f"[green]{summary['passed']}[/green]/[bold]{summary['selected']}[/bold] "
            f"[yellow]{summary['pending']} pending[/yellow]"
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
    return "[#8bd5ca]" + ("█" * filled) + "[/][#3b424a]" + ("░" * empty) + "[/]"


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
        "running": "bold #a6e3d7",
        "done": "bold green",
        "failed": "bold red",
        "cancelled": "bold yellow",
        "created": "#c7baaa",
    }
    return f"[{styles.get(status, '#ded4c8')}]{escape(status)}[/]"


def _status_text(status: str) -> Text:
    styles = {
        "running": "bold #a6e3d7",
        "done": "bold green",
        "failed": "bold red",
        "cancelled": "bold yellow",
        "created": "#c7baaa",
    }
    label = status.upper()[:9]
    return Text(label, style=styles.get(status, "#ded4c8"))


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
