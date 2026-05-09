from __future__ import annotations

import ast
import json
import os
import queue
import re
import shlex
import subprocess
import textwrap
import threading
import time
from pathlib import Path
from typing import Any, Callable, Optional
from urllib.parse import urlparse

from rich.align import Align
from rich.console import Group
from rich.markup import escape
from rich.markdown import Markdown
from rich.padding import Padding
from rich.text import Text
from textual import events
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import DataTable, Input, RichLog, Static, TextArea

from llm_browser.agent import SessionManager
from llm_browser.browser import browser_runtime_diagnostics
from llm_browser.brand import PRODUCT_NAME
from llm_browser.config import apply_config_environment, redacted_config, write_config_values
from llm_browser.datasets import build_dataset_prompt, load_dataset, load_manifest, select_tasks, summarize_manifest
from llm_browser.events import Event
from llm_browser.llm.registry import default_model_for_provider, models_for_provider, provider_names, provider_palette
from llm_browser.provider.base import Provider
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.session.usage import format_cost, format_tokens, summarize_usage_events
from llm_browser.tui.simple import format_event


ProviderFactory = Callable[[], Optional[Provider]]


COMMAND_PALETTE: list[tuple[str, str, str]] = [
    ("New task", "", "Type a plain request and press enter"),
    ("Dataset sample", "dataset real_v8 1", "Run one real_v8 dataset task"),
    ("Dataset by task id", "dataset real_v8 --task-id ", "Start a specific dataset task"),
    ("Keyboard shortcuts", "keys", "Open the keyboard control map"),
    ("Resume selected", "resume", "Continue from the selected session"),
    ("Compact selected", "compact", "Checkpoint and compact selected session"),
    ("Cancel selected", "cancel", "Interrupt the selected session"),
    ("Trace selected", "trace", "Write a trace bundle artifact"),
    ("Self eval", "eval", "Start a self-evaluation child session"),
    ("Report run", "report", "Summarize the selected dataset run"),
    ("Toggle inspector", "inspect", "Show or hide session details and artifacts"),
    ("Toggle tool output", "tools", "Show or hide raw tool output in the timeline"),
    ("Open artifact", "open", "Open the selected artifact"),
    ("Refresh", "refresh", "Reload sessions and artifacts"),
    ("Clear transcript", "clear", "Clear the visible transcript"),
    ("Browser config", "browser", "Show browser runtime details"),
    ("Browser mode", "browser-mode", "Choose auto, chromium, real, or remote browser"),
    ("Authentication", "auth", "Manage provider login and API keys"),
    ("Config", "config", "Show redacted app configuration"),
]


SLASH_COMMANDS: list[tuple[str, str, str]] = [
    ("model", "model", "Select model"),
    ("provider", "provider", "Connect provider"),
    ("browser", "browser", "Select browser backend"),
    ("settings", "settings", "Open settings"),
    ("help", "help", "Help"),
    ("keys", "keys", "Keyboard shortcuts"),
    ("sessions", "sessions", "Switch session"),
    ("new", "new", "Start a new task"),
    ("resume", "resume", "Continue selected session"),
    ("compact", "compact", "Checkpoint selected session"),
    ("cancel", "cancel", "Interrupt selected session"),
    ("clear", "clear", "Clear transcript"),
    ("refresh", "refresh", "Reload sessions and artifacts"),
    ("trace", "trace", "Write trace artifact"),
    ("eval", "eval", "Start self-evaluation session"),
    ("report", "report", "Summarize selected dataset run"),
    ("inspect", "inspect", "Toggle session details"),
    ("artifacts", "artifacts", "Show artifacts"),
    ("tools", "tools", "Toggle tool output"),
    ("open", "open", "Open selected artifact"),
    ("auth", "auth", "Manage auth"),
    ("config", "config", "Show redacted config"),
    ("set", "set ", "Persist a setting"),
    ("dataset", "dataset ", "Run dataset task"),
    ("run", "run ", "Run a new task"),
    ("exit", "exit", "Exit the app"),
]


_ACTIVITY_FRAMES = ("∙", "●", "∙", "·")
_TRANSCRIPT_FOLLOW_MIN_MARGIN = 1
_TRANSCRIPT_FOLLOW_MAX_MARGIN = 2
_BROWSER_ACTION_PREFIX = "browse "
_BROWSER_ACTION_STYLES = {
    "live": "#5c9cf5",
    "open": "#5c9cf5",
    "wait": "#d7a84f",
    "watch": "#d7a84f",
    "tap": "#e0719b",
    "type": "#59b879",
    "key": "#59b879",
    "scroll": "#a78bfa",
    "snap": "#a78bfa",
    "peek": "#8aa6a0",
    "fetch": "#8aa6a0",
    "tabs": "#8aa6a0",
    "steps": "#9b9ca5",
}
_BROWSER_ACTION_ICONS = {
    "live": "●",
    "open": "↗",
    "wait": "…",
    "watch": "◌",
    "tap": "•",
    "type": "⌨",
    "key": "⌘",
    "scroll": "↕",
    "snap": "▣",
    "peek": "◇",
    "fetch": "↓",
    "tabs": "▤",
    "steps": "›",
}


SHORTCUT_PALETTE: list[tuple[str, str, str]] = [
    ("tab", "sessions", "Switch sessions"),
    ("ctrl+p", "", "Open command palette"),
    ("f1", "", "Open this shortcut map"),
    ("f2", "inspect", "Show or hide inspector"),
    ("f3", "artifacts", "Focus artifacts"),
    ("ctrl+o", "tools", "Toggle raw tool output"),
    ("enter", "", "Submit composer"),
    ("shift+enter", "", "Insert composer newline"),
    ("esc", "cancel", "Interrupt running session or close inspector"),
    ("ctrl+r", "refresh", "Refresh sessions and artifacts"),
    ("ctrl+l", "clear", "Clear transcript"),
    ("o", "open", "Open selected artifact"),
    ("ctrl+q", "", "Quit anywhere"),
]


BROWSER_MODE_PALETTE: list[tuple[str, str, str]] = [
    ("Auto", "browser auto", "Use the configured default browser backend"),
    ("Chromium", "browser chromium", "Launch a local Chromium instance"),
    ("Real Chrome", "browser real", "Attach to a running desktop Chrome/Edge/Brave browser"),
    ("Remote", "browser remote", "Use a Browser Use cloud browser"),
    ("CDP", "browser cdp", "Attach to a DevTools endpoint from env or CLI config"),
]


PROVIDER_PALETTE: list[tuple[str, str, str]] = [
    *provider_palette(),
]


API_KEY_PROVIDERS = ("openai", "anthropic", "zai", "qwen", "openrouter")
AUTH_PROVIDER_ALIASES = {
    "openai": "openai",
    "anthropic": "anthropic",
    "claude": "anthropic",
    "zai": "zai",
    "z.ai": "zai",
    "glm": "zai",
    "qwen": "qwen",
    "openrouter": "openrouter",
    "or": "openrouter",
}
AUTH_PROVIDER_LABELS = {
    "openai": "OpenAI",
    "anthropic": "Anthropic",
    "zai": "Z.ai",
    "qwen": "Qwen",
    "openrouter": "OpenRouter",
}


BROWSER_USE_WORDMARK = [
    "▄                                                        ",
    "█▀▀▄ █▀▀▄ █▀▀█ █  █ █▀▀▀ █▀▀▀ █▀▀▄   █  █ █▀▀▀ █▀▀▀",
    "█__█ █__  █__█ █^^█ ▀▀▀█ █___ █__    █__█ ▀▀▀█ █^^^",
    "▀▀▀  ▀    ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀       ▀▀▀▀ ▀▀▀▀ ▀▀▀▀",
]
BROWSER_USE_WORDMARK_SPLIT = 36

def _centered_wordmark_line(line: str, *, canvas_width: int) -> Align:
    raw = line.rstrip()
    left = raw[:BROWSER_USE_WORDMARK_SPLIT]
    right = raw[BROWSER_USE_WORDMARK_SPLIT:]
    text = Text(" ")
    text.append(left, style="#7a7d86")
    text.append(right, style="#e4e4e7")
    return Align.center(text, width=canvas_width)


def _move_table_cursor(table: DataTable, target: int | str) -> None:
    if table.row_count <= 0:
        return
    if target == "home":
        next_row = 0
    elif target == "end":
        next_row = table.row_count - 1
    else:
        next_row = table.cursor_row + target
    next_row = max(0, min(table.row_count - 1, int(next_row)))
    table.move_cursor(row=next_row, column=0)


class ModalFilterInput(Input):
    def on_key(self, event: events.Key) -> None:
        key = event.key
        action = None
        if key in {"escape", "ctrl+c"}:
            action = "action_close"
        elif key in {"up", "ctrl+p"} or (key == "k" and not self.value):
            action = "action_cursor_up"
        elif key in {"down", "ctrl+n"} or (key == "j" and not self.value):
            action = "action_cursor_down"
        elif key == "pageup":
            action = "action_page_up"
        elif key == "pagedown":
            action = "action_page_down"
        elif key == "home" or (key == "g" and not self.value):
            action = "action_cursor_home"
        elif key == "end" or (key == "G" and not self.value):
            action = "action_cursor_end"
        elif key == "enter":
            action = "action_select"
        if action is None:
            return
        handler = getattr(self.screen, action, None)
        if handler is None:
            return
        event.prevent_default()
        event.stop()
        handler()


class ComposerInput(TextArea):
    def on_key(self, event: events.Key) -> None:
        if not getattr(self.app, "_slash_panel_visible", lambda: False)():
            if event.key == "ctrl+c":
                event.prevent_default()
                event.stop()
                self.app.action_composer_ctrl_c()
            elif event.key in {"alt+backspace", "option+backspace", "alt+delete", "option+delete", "ctrl+w"}:
                event.prevent_default()
                event.stop()
                self.action_delete_word_left()
                self.app.resize_composer()
            elif event.key == "pageup":
                event.prevent_default()
                event.stop()
                self.app.action_scroll_transcript_page_up()
            elif event.key == "pagedown":
                event.prevent_default()
                event.stop()
                self.app.action_scroll_transcript_page_down()
            elif event.key == "up" and not self.text:
                event.prevent_default()
                event.stop()
                self.app.action_scroll_transcript_up()
            elif event.key == "down" and not self.text:
                event.prevent_default()
                event.stop()
                self.app.action_scroll_transcript_down()
            elif event.key == "enter":
                event.prevent_default()
                event.stop()
                self.app.submit_composer()
            elif event.key in {"shift+enter", "alt+enter", "ctrl+enter", "cmd+enter", "ctrl+j"}:
                event.prevent_default()
                event.stop()
                self.app.insert_composer_newline()
            return
        key = event.key
        action = None
        if key in {"escape", "ctrl+c"}:
            action = "hide_slash_panel"
        elif key in {"up", "ctrl+p"}:
            action = "slash_cursor_up"
        elif key in {"down", "ctrl+n"}:
            action = "slash_cursor_down"
        elif key == "pageup":
            action = "slash_page_up"
        elif key == "pagedown":
            action = "slash_page_down"
        elif key in {"shift+enter", "alt+enter", "ctrl+enter", "cmd+enter", "ctrl+j"}:
            action = "insert_composer_newline"
        elif key == "enter":
            action = "select_slash_command"
        if action is None:
            return
        handler = getattr(self.app, action, None)
        if handler is None:
            return
        event.prevent_default()
        event.stop()
        handler()

    def on_blur(self) -> None:
        refocus = getattr(self.app, "_refocus_composer_soon", None)
        if refocus is not None:
            refocus()

    def _on_mouse_scroll_up(self, event: events.MouseScrollUp) -> None:
        event.prevent_default()
        event.stop()
        self.app.action_scroll_transcript_up()

    def _on_mouse_scroll_down(self, event: events.MouseScrollDown) -> None:
        event.prevent_default()
        event.stop()
        self.app.action_scroll_transcript_down()

    def _scroll_up_for_pointer(self, **_: Any) -> bool:
        self.app.action_scroll_transcript_up()
        return True

    def _scroll_down_for_pointer(self, **_: Any) -> bool:
        self.app.action_scroll_transcript_down()
        return True

    def _on_scroll_up(self, event: Any) -> None:
        event.prevent_default()
        event.stop()
        self.app.action_scroll_transcript_page_up()

    def _on_scroll_down(self, event: Any) -> None:
        event.prevent_default()
        event.stop()
        self.app.action_scroll_transcript_page_down()


class TranscriptLog(RichLog):
    def _on_mouse_scroll_up(self, event: events.MouseScrollUp) -> None:
        self.auto_scroll = False
        super()._on_mouse_scroll_up(event)

    def _on_mouse_scroll_down(self, event: events.MouseScrollDown) -> None:
        self.auto_scroll = False
        super()._on_mouse_scroll_down(event)


class CommandPalette(ModalScreen[Optional[str]]):
    CSS = """
    CommandPalette {
        align: center middle;
        background: #15161a 94%;
    }

    #palette {
        width: 94;
        max-width: 94%;
        height: 24;
        max-height: 88%;
        padding: 1 2;
        background: #20222b;
        border: round #6f727d;
    }

    #palette-head {
        height: 1;
        margin-bottom: 1;
    }

    #palette-title {
        width: 1fr;
        color: #e4e4e7;
        text-style: bold;
    }

    #palette-esc {
        width: auto;
        padding: 0 1;
        background: #3a3d47;
        color: #e4e4e7;
    }

    #palette-filter {
        height: 1;
        margin-bottom: 1;
        padding: 0 1;
        background: #191b22;
        color: #e4e4e7;
        border: none;
    }

    #palette-table {
        height: 15;
        background: #20222b;
        color: #e4e4e7;
        scrollbar-size: 1 0;
        scrollbar-color: #6f727d;
        scrollbar-background: #20222b;
    }

    #palette-table > .datatable--cursor {
        background: #4a4d57;
        color: #f0f0f2;
        text-style: bold;
    }
    """

    BINDINGS = [
        Binding("escape", "close", "Close", priority=True),
        Binding("ctrl+c", "close", "Close", priority=True),
        Binding("up,k,ctrl+p", "cursor_up", "Up", show=False, priority=True),
        Binding("down,j,ctrl+n", "cursor_down", "Down", show=False, priority=True),
        Binding("pageup", "page_up", "Page up", show=False, priority=True),
        Binding("pagedown", "page_down", "Page down", show=False, priority=True),
        Binding("home,g", "cursor_home", "First", show=False, priority=True),
        Binding("end,G", "cursor_end", "Last", show=False, priority=True),
        Binding("enter", "select", "Select", show=False, priority=True),
    ]

    def __init__(
        self,
        commands: list[tuple[str, str, str]],
        title: str = "Commands",
        placeholder: str = "Search commands",
    ) -> None:
        super().__init__()
        self.commands = commands
        self.title_text = title
        self.placeholder = placeholder
        self._visible_commands: list[str] = []

    def compose(self) -> ComposeResult:
        with Container(id="palette"):
            with Horizontal(id="palette-head"):
                yield Static(self.title_text, id="palette-title")
                yield Static("esc", id="palette-esc")
            yield ModalFilterInput(placeholder=self.placeholder, id="palette-filter", compact=True)
            table = DataTable(
                id="palette-table",
                cursor_type="row",
                show_header=False,
                show_row_labels=False,
                cell_padding=0,
            )
            table.add_column("command", width=88)
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
            self.dismiss(self._command_for_cursor(table))
        elif value:
            self.dismiss(value)

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        self.dismiss(self._command_for_cursor(event.data_table))

    def action_close(self) -> None:
        self.dismiss(None)

    def action_cursor_up(self) -> None:
        _move_table_cursor(self.query_one("#palette-table", DataTable), -1)

    def action_cursor_down(self) -> None:
        _move_table_cursor(self.query_one("#palette-table", DataTable), 1)

    def action_page_up(self) -> None:
        _move_table_cursor(self.query_one("#palette-table", DataTable), -8)

    def action_page_down(self) -> None:
        _move_table_cursor(self.query_one("#palette-table", DataTable), 8)

    def action_cursor_home(self) -> None:
        _move_table_cursor(self.query_one("#palette-table", DataTable), "home")

    def action_cursor_end(self) -> None:
        _move_table_cursor(self.query_one("#palette-table", DataTable), "end")

    def action_select(self) -> None:
        table = self.query_one("#palette-table", DataTable)
        if table.row_count:
            self.dismiss(self._command_for_cursor(table))
            return
        value = self.query_one("#palette-filter", Input).value.strip()
        if value:
            self.dismiss(value)

    def _command_for_cursor(self, table: DataTable) -> str:
        if not self._visible_commands:
            return ""
        index = max(0, min(table.cursor_row, len(self._visible_commands) - 1))
        return self._visible_commands[index]

    def _populate(self, query: str) -> None:
        table = self.query_one("#palette-table", DataTable)
        table.clear()
        self._visible_commands = []
        needle = query.strip().lower()
        for title, command, description in self.commands:
            searchable = f"{title} {command} {description}".lower()
            if needle and needle not in searchable:
                continue
            self._visible_commands.append(command)
            row = Text("❯ ", style="#6f727d")
            row.append(f"{title:<28}", style="bold #e4e4e7")
            row.append(description, style="#9b9ca5")
            table.add_row(row, key=f"command-{len(self._visible_commands) - 1}")


class SecretInputScreen(ModalScreen[Optional[str]]):
    CSS = """
    SecretInputScreen {
        align: center middle;
        background: #15161a 94%;
    }

    #secret-dialog {
        width: 76;
        max-width: 92%;
        height: auto;
        padding: 1 2;
        background: #20222b;
        border: round #6f727d;
    }

    #secret-title {
        color: #e4e4e7;
        text-style: bold;
        margin-bottom: 1;
    }

    #secret-prompt {
        color: #9b9ca5;
        margin-bottom: 1;
    }

    #secret-input {
        height: 1;
        padding: 0 1;
        background: #191b22;
        color: #e4e4e7;
        border: none;
    }
    """

    BINDINGS = [
        Binding("escape", "close", "Close", priority=True),
        Binding("ctrl+c", "close", "Close", priority=True),
    ]

    def __init__(self, title: str, prompt: str, placeholder: str = "Paste key") -> None:
        super().__init__()
        self.title_text = title
        self.prompt = prompt
        self.placeholder = placeholder

    def compose(self) -> ComposeResult:
        with Container(id="secret-dialog"):
            yield Static(self.title_text, id="secret-title")
            yield Static(self.prompt, id="secret-prompt")
            yield Input(placeholder=self.placeholder, password=True, id="secret-input", compact=True)

    def on_mount(self) -> None:
        self.query_one("#secret-input", Input).focus()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        event.stop()
        self.dismiss(event.value.strip())

    def action_close(self) -> None:
        self.dismiss(None)


class SessionPalette(ModalScreen[Optional[str]]):
    CSS = """
    SessionPalette {
        align: center middle;
        background: #15161a 94%;
    }

    #sessions-dialog {
        width: 94;
        max-width: 94%;
        height: 22;
        max-height: 88%;
        padding: 1 2;
        background: #20222b;
        border: round #6f727d;
    }

    #sessions-head {
        height: 1;
        margin-bottom: 1;
    }

    #sessions-dialog-title {
        width: 1fr;
        color: #e4e4e7;
        text-style: bold;
    }

    #sessions-esc {
        width: auto;
        padding: 0 1;
        background: #343746;
        color: #e4e4e7;
    }

    #sessions-filter {
        height: 1;
        margin-bottom: 1;
        padding: 0 1;
        background: #15161a;
        color: #e4e4e7;
        border: none;
    }

    #sessions-table {
        height: 13;
        background: #20222b;
        color: #e4e4e7;
        scrollbar-size: 1 0;
        scrollbar-color: #6f727d;
        scrollbar-background: #20222b;
    }

    #sessions-table > .datatable--cursor {
        background: #4a4d57;
        color: #e4e4e7;
        text-style: bold;
    }
    """

    BINDINGS = [
        Binding("escape", "close", "Close", priority=True),
        Binding("ctrl+c", "close", "Close", priority=True),
        Binding("up,k,ctrl+p", "cursor_up", "Up", show=False, priority=True),
        Binding("down,j,ctrl+n", "cursor_down", "Down", show=False, priority=True),
        Binding("pageup", "page_up", "Page up", show=False, priority=True),
        Binding("pagedown", "page_down", "Page down", show=False, priority=True),
        Binding("home,g", "cursor_home", "First", show=False, priority=True),
        Binding("end,G", "cursor_end", "Last", show=False, priority=True),
        Binding("enter", "select", "Select", show=False, priority=True),
    ]

    def __init__(self, rows: list[tuple[str, str, str, str, str]]) -> None:
        super().__init__()
        self.rows = rows
        self._visible_session_ids: list[str] = []

    def compose(self) -> ComposeResult:
        with Container(id="sessions-dialog"):
            with Horizontal(id="sessions-head"):
                yield Static("Sessions", id="sessions-dialog-title")
                yield Static("esc", id="sessions-esc")
            yield ModalFilterInput(placeholder="Search sessions", id="sessions-filter", compact=True)
            table = DataTable(
                id="sessions-table",
                cursor_type="row",
                show_header=False,
                show_row_labels=False,
                cell_padding=0,
            )
            table.add_column("session", width=88)
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

    def action_cursor_up(self) -> None:
        _move_table_cursor(self.query_one("#sessions-table", DataTable), -1)

    def action_cursor_down(self) -> None:
        _move_table_cursor(self.query_one("#sessions-table", DataTable), 1)

    def action_page_up(self) -> None:
        _move_table_cursor(self.query_one("#sessions-table", DataTable), -8)

    def action_page_down(self) -> None:
        _move_table_cursor(self.query_one("#sessions-table", DataTable), 8)

    def action_cursor_home(self) -> None:
        _move_table_cursor(self.query_one("#sessions-table", DataTable), "home")

    def action_cursor_end(self) -> None:
        _move_table_cursor(self.query_one("#sessions-table", DataTable), "end")

    def action_select(self) -> None:
        table = self.query_one("#sessions-table", DataTable)
        if table.row_count:
            self.dismiss(self._visible_session_ids[table.cursor_row])

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
            task_label = _compact_inline(task or session_id, limit=56)
            row = Text("❯ ", style="#6f727d")
            row.append(f"{task_label:<58}", style="bold #e4e4e7")
            row.append(f"{status[:9]:<10}", style=_status_text(status).style)
            row.append(f"{age:<8}", style="#9b9ca5")
            if run != "-":
                row.append(run, style="#9b9ca5")
            table.add_row(row, key=session_id)


class BrowserUseTerminalApp(App[None]):
    CSS = """
    Screen {
        background: transparent;
        color: #e4e4e7;
    }

    #body {
        height: 1fr;
        background: transparent;
    }

    #main {
        width: 1fr;
        min-width: 52;
        padding: 1 2;
        background: transparent;
        align: center middle;
    }

    #main.home {
        padding: 0 0 2 0;
        align: center middle;
    }

    #workspace {
        width: 1fr;
        max-width: 100%;
        height: 1fr;
    }

    #main.home #workspace {
        width: 76;
        max-width: 94%;
        height: auto;
    }

    #home-logo {
        display: none;
        height: auto;
        margin-bottom: 2;
        background: transparent;
    }

    #main.home #home-logo {
        display: block;
        height: auto;
    }

    #transcript {
        height: 1fr;
        padding: 0 1;
        background: transparent;
        color: #e4e4e7;
        scrollbar-color: #6f727d;
        scrollbar-background: transparent;
        scrollbar-gutter: stable;
        scrollbar-size-horizontal: 0;
        scrollbar-size-vertical: 1;
    }

    #main.home #transcript {
        display: none;
        height: auto;
        min-height: 20;
        max-height: 36;
        padding: 0 0;
        scrollbar-size: 0 0;
    }

    #slash-panel {
        height: 8;
        margin-top: 2;
        padding: 0 1;
        background: #1e1e1e;
        color: #e4e4e7;
        border: none;
        scrollbar-size-horizontal: 0;
        scrollbar-size-vertical: 0;
        scrollbar-color: #6f727d;
        scrollbar-background: #1e1e1e;
    }

    #activity {
        display: none;
        height: auto;
        max-height: 3;
        margin-top: 1;
        padding: 0 1;
        background: #15161a;
        color: #9b9ca5;
    }

    #runtime-meta {
        display: none;
        height: 1;
        margin-top: 2;
        color: #6f727d;
        content-align-horizontal: center;
    }

    #main.home #runtime-meta {
        display: block;
    }

    #composer {
        height: 3;
        margin-top: 2;
        padding: 1 2;
        background: #1c1d22;
        border: none;
    }

    #composer:focus-within {
        background: #20222a;
    }

    #main.home #composer {
        border-left: solid #5c9cf5;
    }

    #command {
        height: 1;
        border: none;
        background: transparent;
        color: #e4e4e7;
        scrollbar-size: 0 0;
    }

    #composer-meta {
        display: none;
        height: 0;
        margin: 0;
        color: #9b9ca5;
    }

    #hintbar {
        height: 2;
        margin-top: 1;
        color: #9b9ca5;
        padding: 0 1 0 1;
        content-align-vertical: middle;
    }

    #main.home #hintbar {
        content-align-horizontal: right;
    }

    #sidebar {
        width: 44;
        min-width: 38;
        padding: 1 2;
        background: #191b22;
        border: none;
    }

    #session-detail {
        height: auto;
        max-height: 15;
        color: #e4e4e7;
        background: #191b22;
    }

    #artifacts-title, #preview-title {
        height: 1;
        margin-top: 1;
        color: #e4e4e7;
        text-style: bold;
    }

    #artifacts {
        height: 8;
        background: #191b22;
        color: #e4e4e7;
        scrollbar-size: 0 0;
    }

    #artifact-preview {
        height: 1fr;
        min-height: 12;
        margin-top: 1;
        background: #191b22;
        color: #9b9ca5;
        scrollbar-color: #6f727d;
        scrollbar-background: #191b22;
        scrollbar-size: 0 0;
    }

    #sidebar-footer {
        display: none;
        height: 0;
        color: #9b9ca5;
    }

    DataTable {
        background: #191b22;
        color: #e4e4e7;
        scrollbar-color: #6f727d;
        scrollbar-background: #191b22;
    }

    #slash-panel {
        background: #1e1e1e;
    }

    DataTable > .datatable--cursor {
        background: #3a3d47;
        color: #e4e4e7;
        text-style: bold;
    }

    DataTable:focus > .datatable--cursor {
        background: #4a4d57;
        color: #e4e4e7;
        text-style: bold;
    }

    #artifacts > .datatable--cursor {
        background: #3a3d47;
        color: #e4e4e7;
        text-style: bold;
    }

    #artifacts:focus > .datatable--cursor {
        background: #4a4d57;
        color: #e4e4e7;
        text-style: bold;
    }

    Input > .input--cursor {
        background: #e4e4e7;
        color: #20222b;
    }

    Input > .input--placeholder {
        color: #6f727d;
    }

    TextArea > .text-area--placeholder {
        color: #6f727d;
    }
    """

    BINDINGS = [
        Binding("escape", "cancel_selected", "Interrupt", priority=True),
        Binding("ctrl+q", "quit", "Quit", priority=True),
        Binding("tab", "show_sessions", "Sessions", priority=True),
        Binding("ctrl+p", "show_commands", "Commands", priority=True),
        Binding("f1", "show_shortcuts", "Keys", priority=True),
        Binding("pageup", "scroll_transcript_page_up", "Scroll up", show=False),
        Binding("pagedown", "scroll_transcript_page_down", "Scroll down", show=False),
        Binding("ctrl+r", "refresh", "Refresh"),
        Binding("ctrl+l", "clear_log", "Clear"),
        Binding("ctrl+o", "toggle_tool_details", "Tools"),
        Binding("f2", "toggle_inspector", "Inspector"),
        Binding("f3", "focus_artifacts", "Artifacts"),
        Binding("o", "open_artifact", "Open"),
    ]

    def __init__(
        self,
        store: SessionStore,
        provider_factory: Optional[ProviderFactory] = None,
        max_turns: int = 80,
        provider_label: str = "fake",
        model_label: Optional[str] = None,
        config: Optional[dict] = None,
        config_path: Optional[Path | str] = None,
    ) -> None:
        super().__init__()
        self.store = store
        self._external_provider_factory = provider_factory
        self.provider_label = provider_label
        self.model_label = model_label
        self.config = config or {}
        self.config_path = Path(config_path).expanduser() if config_path else None
        apply_config_environment(self.config)
        self.manager = SessionManager(store, provider_factory=self._make_provider_for_current_settings, max_turns=max_turns)
        self.selected_session_id: Optional[str] = None
        self.selected_artifact_path: Optional[str] = None
        self._visible_slash_commands: list[tuple[str, str, str]] = []
        self._preview_key: Optional[tuple[str, float, int]] = None
        self._model_buffers: dict[str, str] = {}
        self._last_transcript_text: dict[str, str] = {}
        self._recent_model_text: dict[str, str] = {}
        self._recent_model_stream_text: dict[str, str] = {}
        self._recent_tool_output_text: dict[tuple[str, str], str] = {}
        self._rendered_event_ids: set[str] = set()
        self._last_transcript_event_type: Optional[str] = None
        self._home_mode = False
        self._inspector_visible = False
        self._tool_details_visible = False
        self._last_composer_ctrl_c = 0.0
        self._quit_hint_until = 0.0
        self._activity_frame = 0
        self._stop = threading.Event()
        self._listener: Optional[threading.Thread] = None
        self._pending_secret: Optional[tuple[str, str]] = None

    def compose(self) -> ComposeResult:
        with Horizontal(id="body"):
            with Vertical(id="main"):
                with Vertical(id="workspace"):
                    yield Static("", id="home-logo")
                    yield TranscriptLog(id="transcript", wrap=True, highlight=False, markup=True)
                    yield Static("", id="activity")
                    yield Static("", id="runtime-meta")
                    slash_panel = DataTable(
                        id="slash-panel",
                        cursor_type="row",
                        show_header=False,
                        show_row_labels=False,
                        cell_padding=0,
                    )
                    slash_panel.add_column("command", width=22)
                    slash_panel.add_column("description", width=72)
                    yield slash_panel
                    with Vertical(id="composer"):
                        yield ComposerInput(
                            "",
                            placeholder=' Ask anything... "Find the page and save a screenshot"',
                            id="command",
                            compact=True,
                            soft_wrap=True,
                            show_line_numbers=False,
                            highlight_cursor_line=False,
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
                    cell_padding=0,
                )
                artifacts.add_column("kind", width=6)
                artifacts.add_column("name", width=24)
                artifacts.add_column("size", width=8)
                yield artifacts
                yield Static("preview", id="preview-title")
                yield RichLog(id="artifact-preview", wrap=True, highlight=False, markup=True)
                yield Static("", id="sidebar-footer")

    def on_mount(self) -> None:
        try:
            self.theme = "textual-dark"
        except Exception:
            pass
        self.title = PRODUCT_NAME
        self.sub_title = "raw CDP browser agent"
        self.refresh_sessions()
        self._write_home()
        self.refresh_artifacts()
        self._update_statusbar()
        self._update_session_detail()
        self._update_activity_strip()
        self.query_one("#slash-panel", DataTable).display = False
        self.query_one("#artifact-preview", RichLog).auto_scroll = False
        self._sync_sidebar_visibility()
        self.query_one("#command", ComposerInput).focus()
        self.resize_composer()
        self.call_after_refresh(self._rewrite_home_after_layout)
        self._listener = threading.Thread(target=self._listen_events, name="browser-use-terminal-events", daemon=True)
        self._listener.start()
        self.set_interval(1.0, self._tick)

    def on_unmount(self) -> None:
        self._stop.set()

    def on_resize(self, event: events.Resize) -> None:
        if self._home_mode and not self.selected_session_id:
            self._write_home()

    def _rewrite_home_after_layout(self) -> None:
        if self._home_mode and not self.selected_session_id:
            self._write_home()

    def on_click(self, event: events.Click) -> None:
        pass

    def on_mouse_scroll_up(self, event: events.MouseScrollUp) -> None:
        if self._mouse_event_targets_transcript_scroll(event):
            event.prevent_default()
            event.stop()
            self.action_scroll_transcript_up()

    def on_mouse_scroll_down(self, event: events.MouseScrollDown) -> None:
        if self._mouse_event_targets_transcript_scroll(event):
            event.prevent_default()
            event.stop()
            self.action_scroll_transcript_down()

    def _mouse_event_targets_transcript_scroll(self, event: events.MouseEvent) -> bool:
        widget = event.widget
        while widget is not None:
            if widget.id == "sidebar":
                return False
            if widget.id in {"main", "workspace", "transcript", "composer", "command"}:
                return True
            widget = widget.parent
        try:
            return self.focused is self.query_one("#command", ComposerInput)
        except Exception:
            return False

    def action_quit(self) -> None:
        self.exit()

    def action_composer_ctrl_c(self) -> None:
        now = time.monotonic()
        if now - self._last_composer_ctrl_c <= 1.5:
            self.exit()
            return
        self._set_composer_text("")
        self.hide_slash_panel()
        self._last_composer_ctrl_c = now
        self._quit_hint_until = now + 1.5
        self._update_statusbar()
        self._refocus_composer_soon()

    def _refocus_composer_soon(self) -> None:
        try:
            if isinstance(self.screen, ModalScreen):
                return
        except Exception:
            return

        def focus_composer() -> None:
            try:
                if isinstance(self.screen, ModalScreen):
                    return
            except Exception:
                return
            try:
                self.query_one("#command", ComposerInput).focus()
            except Exception:
                return

        self.set_timer(0.01, focus_composer)

    def _make_provider_for_current_settings(self) -> Optional[Provider]:
        if self.provider_label == "fake":
            return None
        from llm_browser.provider.factory import make_provider

        try:
            return make_provider(self.provider_label, self.model_label, config=self.config)
        except Exception:
            if self._external_provider_factory is not None:
                return self._external_provider_factory()
            raise

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
        if not self._ui_ready():
            return
        self.refresh_sessions()
        self.refresh_artifacts()
        self._update_statusbar()
        self._update_session_detail()
        self._update_activity_strip()

    def _ui_ready(self) -> bool:
        try:
            self.query_one("#home-logo", Static)
            self.query_one("#composer-meta", Static)
            self.query_one("#runtime-meta", Static)
        except Exception:
            return False
        return True

    def _set_home_mode(self, enabled: bool) -> None:
        main = self.query_one("#main")
        main.set_class(enabled, "home")
        self._home_mode = enabled
        try:
            transcript = self.query_one("#transcript", RichLog)
            transcript.display = not enabled
            transcript.wrap = not enabled
            self.query_one("#home-logo", Static).display = enabled
        except Exception:
            pass
        self._sync_sidebar_visibility()

    def _sync_sidebar_visibility(self) -> None:
        self.query_one("#sidebar").display = bool(not self._home_mode and self._inspector_visible)

    def _write_home(self) -> None:
        self._set_home_mode(True)
        self._update_activity_strip()
        log = self.query_one("#transcript", RichLog)
        log.clear()
        self._last_transcript_event_type = None
        logo = self.query_one("#home-logo", Static)
        available_width = int(logo.size.width or self.query_one("#workspace").size.width or self.size.width or 128)
        canvas_width = max(58, min(76, available_width))
        renderables = [
            _centered_wordmark_line(line, canvas_width=canvas_width)
            for line in BROWSER_USE_WORDMARK
        ]
        logo.update(Group(*renderables))
        self._update_statusbar()

    def _write_banner(self) -> None:
        self._set_home_mode(False)
        log = self.query_one("#transcript", RichLog)
        self._sync_transcript_render_width_hint(log)
        self._last_transcript_event_type = None
        log.write("[bold #e4e4e7]Slash commands[/bold #e4e4e7]")
        for name, command, description in SLASH_COMMANDS:
            command_label = f"/{command}".rstrip()
            log.write(f"[#e4e4e7]{escape(command_label):<18}[/] [#9b9ca5]{escape(description)}[/]")
        log.write("[#9b9ca5]Use /settings for model, provider, browser, API keys, viewport, cloud, CDP, and max-turns settings.[/]")
        log.write("[#9b9ca5]Use /auth for provider API keys, Codex login, Anthropic Claude Code login, and auth status.[/]")

    def _handle_event(self, event: Event) -> None:
        should_render = event.session_id == self.selected_session_id and event.id not in self._rendered_event_ids
        if should_render:
            self._rendered_event_ids.add(event.id)

        if event.type == "model.delta":
            if should_render:
                self._append_model_delta(event)
            return

        if should_render:
            self._flush_model_delta(event.session_id)
            for line, event_type in self._format_event_items_for_live_transcript(event):
                if self._should_render_transcript_line(event_type, line) and not self._is_duplicate_terminal_result(
                    event, line
                ):
                    self._write_log_line(line, event_type)
        self.refresh_sessions()
        self.refresh_artifacts()
        self._update_statusbar()
        self._update_session_detail()
        self._update_activity_strip()

    def _format_event_for_live_transcript(self, event: Event) -> str:
        items = self._format_event_items_for_live_transcript(event)
        return items[0][0] if items else ""

    def _format_event_items_for_live_transcript(self, event: Event) -> list[tuple[str, str]]:
        if event.type == "tool.output":
            self._remember_tool_output(event)
        if not self._tool_details_visible:
            if event.type == "tool.finished" and self._tool_finished_repeats_stream(event):
                payload = event.payload
                line = _format_tool_finished_for_transcript(
                    str(payload.get("name") or "tool"),
                    payload.get("output") or {},
                    omit_text=True,
                )
                return [(line, "tool.finished")] if line else []
            return _format_default_transcript_event(event)
        if event.type == "tool.finished" and self._tool_finished_repeats_stream(event):
            payload = event.payload
            line = _format_tool_finished_for_transcript(
                str(payload.get("name") or "tool"),
                payload.get("output") or {},
                omit_text=True,
            )
            return [(line, "tool.finished")] if line else []
        line = _format_event_for_transcript(event)
        return [(line, _transcript_event_type(event))] if line else []

    def _remember_tool_output(self, event: Event) -> None:
        call_id = str(event.payload.get("tool_call_id") or "")
        text = str(event.payload.get("text") or "")
        if not call_id or not text.strip():
            return
        key = (event.session_id, call_id)
        previous = self._recent_tool_output_text.get(key, "")
        self._recent_tool_output_text[key] = _join_transcript_text(previous, text)

    def _tool_finished_repeats_stream(self, event: Event) -> bool:
        call_id = str(event.payload.get("tool_call_id") or "")
        if not call_id:
            return False
        output = event.payload.get("output") or {}
        text = str(output.get("text") or "")
        if not text.strip():
            return False
        previous = self._recent_tool_output_text.get((event.session_id, call_id), "")
        return _canonical_transcript_text(previous) == _canonical_transcript_text(text)

    def _should_render_transcript_line(self, event_type: str, line: str) -> bool:
        if not line:
            return False
        if self._tool_details_visible:
            return True
        if event_type in {"browser.action", "browser.live_url"}:
            return True
        if event_type == "tool.started":
            return line.startswith(_BROWSER_ACTION_PREFIX)
        if event_type == "tool.output":
            return False
        if event_type == "tool.finished":
            return line.startswith("✕")
        return True

    def _append_model_delta(self, event: Event) -> None:
        text = str(event.payload.get("text") or "")
        if not text:
            return
        previous_stream = self._recent_model_stream_text.get(event.session_id, "")
        self._recent_model_stream_text[event.session_id] = (previous_stream + text)[-12000:]
        buffered = self._model_buffers.get(event.session_id, "") + text
        self._model_buffers[event.session_id] = buffered
        if "\n" in buffered or len(buffered) >= 700:
            self._flush_model_delta(event.session_id)

    def _flush_model_delta(self, session_id: str) -> None:
        text = self._model_buffers.pop(session_id, "")
        if not text.strip():
            return
        self._write_log_line(text.strip(), "model.delta")

    def _flush_all_model_deltas(self) -> None:
        for session_id in list(self._model_buffers):
            self._flush_model_delta(session_id)

    def _transcript_follow_margin(self, transcript: RichLog) -> int:
        height = int(transcript.size.height or 0)
        if height <= 0:
            return _TRANSCRIPT_FOLLOW_MIN_MARGIN
        return max(
            _TRANSCRIPT_FOLLOW_MIN_MARGIN,
            min(_TRANSCRIPT_FOLLOW_MAX_MARGIN, height // 10),
        )

    def _should_follow_transcript_updates(self, transcript: RichLog) -> bool:
        if bool(transcript.auto_scroll):
            return True
        max_scroll_y = int(transcript.max_scroll_y or 0)
        if max_scroll_y <= 0:
            return True
        distance_from_bottom = max_scroll_y - int(transcript.scroll_y or 0)
        return distance_from_bottom <= self._transcript_follow_margin(transcript)

    def _write_log_line(self, line: str, event_type: str, *, follow: Optional[bool] = None) -> None:
        if not line:
            return
        if self.selected_session_id and event_type in {
            "model.delta",
            "session.done",
            "session.cancelled",
            "session.failed",
            "tool.finished",
        }:
            self._last_transcript_text[self.selected_session_id] = _canonical_transcript_text(line)
            if event_type == "model.delta":
                previous = self._recent_model_text.get(self.selected_session_id, "")
                self._recent_model_text[self.selected_session_id] = _join_transcript_text(previous, line)
        log = self.query_one("#transcript", RichLog)
        self._sync_transcript_render_width_hint(log)
        log.auto_scroll = self._should_follow_transcript_updates(log) if follow is None else follow
        self._write_transcript_spacing(log, event_type)
        escaped = escape(line)
        if event_type == "session.input":
            log.write(_prompt_text(line, width=_prompt_width(log, fallback=self.size.width)))
        elif event_type == "session.followup":
            log.write(_prompt_text(line, width=_prompt_width(log, fallback=self.size.width)))
        elif event_type == "tool.started":
            _write_tool_started_line(log, line)
        elif event_type == "tool.failed":
            log.write(f"[#e4e4e7]{escaped}[/]")
        elif event_type == "tool.image":
            log.write(f"[#9b9ca5]{escaped}[/]")
        elif event_type == "tool.output":
            log.write(f"[#9b9ca5]{escaped}[/]")
        elif event_type == "tool.finished":
            log.write(f"[#9b9ca5]{escaped}[/]")
        elif event_type == "model.delta":
            _write_transcript_markdown(log, line)
        elif event_type in {"session.done", "session.cancelled"}:
            _write_transcript_markdown(log, line, style="#e4e4e7")
        elif event_type in {"browser.action", "browser.live_url"}:
            _write_browser_action_line(log, line)
        elif event_type == "session.failed":
            log.write(f"[bold #e4e4e7]{escaped}[/bold #e4e4e7]")
        elif event_type == "session.deadline_warning":
            log.write(f"[#9b9ca5]{escaped}[/]")
        else:
            log.write(escaped)
        self._last_transcript_event_type = event_type

    def _write_transcript_spacing(self, log: RichLog, event_type: str) -> None:
        previous = self._last_transcript_event_type
        if previous is None:
            return
        if event_type in {"session.input", "session.followup"}:
            log.write("")
            return
        if previous in {"session.input", "session.followup"}:
            log.write("")
            return
        tool_like = {
            "browser.action",
            "browser.live_url",
            "tool.started",
            "tool.image",
            "tool.output",
            "tool.finished",
            "tool.failed",
        }
        answer_like = {"model.delta", "session.done", "session.failed", "session.cancelled"}
        if previous in tool_like and event_type in answer_like:
            log.write("")

    def _is_duplicate_terminal_result(self, event: Event, line: str) -> bool:
        if event.type != "session.done" or not self.selected_session_id:
            return False
        result = str(event.payload.get("result") or "")
        if not result.strip():
            return False
        previous = self._last_transcript_text.get(self.selected_session_id, "")
        recent_model = self._recent_model_text.get(self.selected_session_id, "")
        recent_stream = self._recent_model_stream_text.get(self.selected_session_id, "")
        canonical_result = _canonical_transcript_text(result)
        return (
            previous == canonical_result
            or previous == _canonical_transcript_text(line)
            or _canonical_transcript_text(recent_model) == canonical_result
            or _canonical_transcript_text(recent_model).endswith(canonical_result)
            or _canonical_transcript_text(recent_stream) == canonical_result
            or _canonical_transcript_text(recent_stream).endswith(canonical_result)
        )

    def _slash_panel_visible(self) -> bool:
        return bool(self.query_one("#slash-panel", DataTable).display)

    def _update_slash_panel(self, value: str) -> None:
        table = self.query_one("#slash-panel", DataTable)
        stripped = value.strip()
        if not stripped.startswith("/") or " " in stripped:
            self.hide_slash_panel()
            return
        needle = stripped[1:].lower()
        matches = [
            row
            for row in SLASH_COMMANDS
            if not needle or needle in row[0].lower() or needle in row[2].lower()
        ][:9]
        table.clear()
        self._visible_slash_commands = matches
        if not matches:
            table.display = False
            return
        table.styles.height = min(len(matches), 8)
        for name, _command, description in matches:
            table.add_row(
                Text(f"/{name}", style="#e4e4e7"),
                Text(description, style="#9b9ca5"),
                key=name,
            )
        table.display = True
        table.move_cursor(row=0, column=0)

    def hide_slash_panel(self) -> None:
        self.query_one("#slash-panel", DataTable).display = False
        self._visible_slash_commands = []

    def slash_cursor_up(self) -> None:
        _move_table_cursor(self.query_one("#slash-panel", DataTable), -1)

    def slash_cursor_down(self) -> None:
        _move_table_cursor(self.query_one("#slash-panel", DataTable), 1)

    def slash_page_up(self) -> None:
        _move_table_cursor(self.query_one("#slash-panel", DataTable), -8)

    def slash_page_down(self) -> None:
        _move_table_cursor(self.query_one("#slash-panel", DataTable), 8)

    def select_slash_command(self) -> None:
        table = self.query_one("#slash-panel", DataTable)
        if not self._visible_slash_commands or table.row_count <= 0:
            self.hide_slash_panel()
            return
        name, command, _description = self._visible_slash_commands[table.cursor_row]
        command_input = self.query_one("#command", ComposerInput)
        self.hide_slash_panel()
        if command.endswith(" "):
            self._set_composer_text(f"/{command}")
            command_input.focus()
            return
        self._set_composer_text("")
        self._handle_command(f"/{command}")
        command_input.focus()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        line = event.value.strip()
        event.input.value = ""
        self.hide_slash_panel()
        if not line:
            return
        if self._consume_pending_secret(line):
            return
        self._handle_command(line)

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "command":
            self._update_slash_panel(event.value)

    def on_text_area_changed(self, event: TextArea.Changed) -> None:
        if event.text_area.id != "command":
            return
        self.resize_composer()
        self._update_slash_panel(event.text_area.text)

    def insert_composer_newline(self) -> None:
        command_input = self.query_one("#command", ComposerInput)
        command_input.insert("\n")
        self.resize_composer()
        self._sync_composer_scroll()

    def submit_composer(self) -> None:
        command_input = self.query_one("#command", ComposerInput)
        line = command_input.text.strip()
        self._set_composer_text("")
        self.hide_slash_panel()
        if not line:
            return
        if self._consume_pending_secret(line):
            return
        if "\n" in line and not line.startswith("/"):
            self._start_or_resume_task(line)
            return
        self._handle_command(line)

    def resize_composer(self) -> None:
        command_input = self.query_one("#command", ComposerInput)
        visible_lines = _composer_visible_line_count(command_input.text, command_input.size.width)
        command_input.styles.height = visible_lines
        self.query_one("#composer").styles.height = visible_lines + 2
        self._sync_composer_scroll()
        self.call_after_refresh(self._sync_composer_scroll)

    def _sync_composer_scroll(self) -> None:
        try:
            command_input = self.query_one("#command", ComposerInput)
        except Exception:
            return
        total_lines = _composer_visible_line_count(command_input.text, command_input.size.width, max_lines=10_000)
        visible_lines = _composer_visible_line_count(command_input.text, command_input.size.width)
        if total_lines <= visible_lines:
            command_input.scroll_to(x=0, y=0, animate=False, force=True, immediate=True)

    def _set_composer_text(self, text: str) -> None:
        command_input = self.query_one("#command", ComposerInput)
        command_input.load_text(text)
        lines = text.splitlines() or [""]
        command_input.move_cursor((len(lines) - 1, len(lines[-1])), center=False)
        self.resize_composer()

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        if event.data_table.id == "artifacts":
            self.selected_artifact_path = str(event.row_key.value)
            self._preview_artifact(self.selected_artifact_path, force=True)
        elif event.data_table.id == "slash-panel":
            self.select_slash_command()

    def _handle_command(self, line: str) -> None:
        log = self.query_one("#transcript", RichLog)
        is_slash_command = line.startswith("/")
        normalized_line = line.lstrip("/")
        if normalized_line.startswith("run "):
            task = normalized_line[4:].strip()
            if not task:
                log.write("[#e4e4e7]run requires a task[/]")
                return
            self._start_task(task)
            return

        try:
            args = shlex.split(normalized_line)
        except ValueError as exc:
            if not is_slash_command:
                self._start_or_resume_task(line)
                return
            log.write(f"[#e4e4e7]parse error: {escape(str(exc))}[/]")
            return
        if not args:
            return

        command = args[0]
        if command in {"quit", "exit"}:
            self.exit()
        elif command == "help":
            self._write_banner()
        elif command in {"keys", "shortcuts", "keyboard"}:
            self.action_show_shortcuts()
        elif command == "new":
            self.selected_session_id = None
            self.selected_artifact_path = None
            self._write_home()
            self.refresh_artifacts()
            self._update_statusbar()
            self._update_session_detail()
        elif command == "refresh":
            self.action_refresh()
        elif command == "clear":
            self.action_clear_log()
        elif command == "sessions":
            self.action_show_sessions()
        elif command in {"inspect", "inspector", "details"}:
            self.action_toggle_inspector()
        elif command in {"tools", "tool-output", "debug"}:
            self.action_toggle_tool_details()
        elif command == "artifacts":
            if not self.selected_session_id:
                log.write("[#7a7d86]no selected session[/]")
                return
            self.action_focus_artifacts()
        elif command == "model":
            if len(args) >= 2:
                self._set_model(args[1])
            else:
                self.action_show_model_selector()
        elif command in {"provider", "connect"}:
            if len(args) >= 2:
                self._set_provider(args[1])
            else:
                self.action_show_provider_selector()
        elif command == "settings":
            self.action_show_settings()
        elif command == "browser":
            if len(args) >= 2 and args[1] in {"config", "detail", "details", "status"}:
                log.write(escape(_browser_runtime_detail()))
            elif len(args) >= 2:
                self._set_browser_mode(args[1])
            else:
                self.action_show_browser_modes()
        elif command in {"browser-mode", "browsermode"}:
            if len(args) >= 2:
                self._set_browser_mode(args[1])
            else:
                self.action_show_browser_modes()
        elif command == "browser-config":
            log.write(escape(_browser_runtime_detail()))
        elif command == "headless":
            if len(args) < 2:
                log.write(f"[#7a7d86]headless is {'on' if _browser_headless_default() else 'off'}[/]")
            else:
                self._set_bool_env("LLM_BROWSER_HEADLESS", args[1], "headless", "browser.headless")
        elif command == "viewport":
            self._set_viewport(args[1:])
        elif command in {"max-turns", "max_turns"}:
            if len(args) < 2:
                log.write(f"[#7a7d86]max turns: {self.manager.max_turns}[/]")
            else:
                self._set_max_turns(args[1])
        elif command == "set":
            self._set_config_value(args[1:])
        elif command == "config":
            payload = {
                "path": str(self.config_path) if self.config_path else None,
                "config": redacted_config(self.config),
            }
            log.write(escape(json.dumps(payload, indent=2)))
        elif command == "auth":
            self._handle_auth_command(args[1:])
        elif command == "report":
            run_id = args[1] if len(args) >= 2 else self._selected_dataset_run_id()
            if not run_id:
                log.write("[#e4e4e7]report requires a run id or a selected dataset session[/]")
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
                log.write("[#e4e4e7]no selected session to resume[/]")
                return
            self._resume_session(session_id, instruction)
        elif command == "compact":
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            if not session_id:
                log.write("[#e4e4e7]no selected session to compact[/]")
                return
            try:
                compacting = self.manager.compact(session_id)
            except Exception as exc:
                log.write(f"[#e4e4e7]compact failed: {escape(str(exc))}[/]")
                return
            self.selected_session_id = compacting.id
            self._load_session_log(compacting.id, follow=False)
            log.write(f"[#9b9ca5]compaction started for {escape(compacting.id)}[/]")
        elif command == "cancel":
            session_id = args[1] if len(args) > 1 else self.selected_session_id
            if not session_id:
                log.write("[#e4e4e7]no selected session to cancel[/]")
                return
            session = self.store.load(session_id)
            if session is None:
                log.write(f"[#e4e4e7]session not found: {escape(session_id)}[/]")
                return
            if session.status not in {"created", "running"}:
                log.write(f"[#7a7d86]session is {escape(session.status)}; nothing to cancel[/]")
                return
            self.manager.cancel(session_id)
            log.write(f"[#9b9ca5]cancel requested for {escape(session_id)}[/]")
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
                        log.write(f"[#e4e4e7]invalid dataset option: {escape(rest[index])}[/]")
                        return
                    index += 1
            tasks = select_tasks(load_dataset(args[1]), count=count, task_ids=task_ids or None)
            for task in tasks:
                session = self.manager.start(build_dataset_prompt(task, headless=_browser_headless_default()))
                self.selected_session_id = session.id
                self._load_session_log(session.id)
        else:
            if is_slash_command:
                log.write(f"[#e4e4e7]unknown command: {escape(command)}[/]")
            else:
                self._start_or_resume_task(line)

    def _start_or_resume_task(self, instruction: str) -> None:
        log = self.query_one("#transcript", RichLog)
        if self.selected_session_id:
            session = self.store.load(self.selected_session_id)
            if session is not None and session.status in {"created", "running"}:
                log.write("[#9b9ca5]selected session is still running; press esc to interrupt or wait for it to finish[/]")
                return
            self._resume_session(self.selected_session_id, instruction)
            return
        self._start_task(instruction)

    def _load_session_log(self, session_id: str, *, follow: Optional[bool] = True) -> None:
        self._set_home_mode(False)
        log = self.query_one("#transcript", RichLog)
        self._sync_transcript_render_width_hint(log)
        follow_transcript = self._should_follow_transcript_updates(log) if follow is None else follow
        log.auto_scroll = follow_transcript
        log.clear()
        self._last_transcript_event_type = None
        session = self.store.load(session_id)
        if session is None:
            log.write(f"[#e4e4e7]session not found: {escape(session_id)}[/]")
            return
        self.selected_session_id = session.id
        self.selected_artifact_path = None
        self._model_buffers.pop(session.id, None)
        self._recent_tool_output_text = {
            key: value for key, value in self._recent_tool_output_text.items() if key[0] != session.id
        }
        events = self.store.events.read(session.id)
        self._rendered_event_ids = {event.id for event in events}
        self._last_transcript_text[session.id] = ""
        self._recent_model_text[session.id] = ""
        self._recent_model_stream_text[session.id] = ""
        for line, event_type in _format_events_for_transcript(events, show_tools=self._tool_details_visible):
            if self._should_render_transcript_line(event_type, line):
                self._write_log_line(line, event_type, follow=follow_transcript)
        self._update_session_detail()
        self._update_activity_strip()

    def _sync_transcript_render_width_hint(self, log: RichLog) -> None:
        widths = [
            int(getattr(log.size, "width", 0) or 0),
            int(getattr(log.content_region, "width", 0) or 0),
            int(getattr(log, "min_width", 0) or 0),
            int(self.size.width or 80) - 6,
        ]
        try:
            workspace = self.query_one("#workspace")
            widths.append(int(workspace.size.width or 0) - 2)
        except Exception:
            pass
        try:
            sidebar = self.query_one("#sidebar")
            if bool(sidebar.display):
                widths.append(int(self.size.width or 80) - int(sidebar.size.width or 44) - 8)
        except Exception:
            pass
        log.min_width = max(78, *(width for width in widths if width > 0))

    def refresh_sessions(self) -> None:
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
                Text(_artifact_kind(path), style="#9b9ca5"),
                _artifact_display_name(session, path),
                Text(_format_bytes(stat.st_size), style="#9b9ca5"),
                key=str(path),
            )
        if self.selected_artifact_path is None:
            self.selected_artifact_path = first_path
        self._preview_artifact(self.selected_artifact_path)

    def action_cancel_selected(self) -> None:
        if isinstance(self.screen, ModalScreen):
            self.screen.dismiss(None)
            return
        if not self.selected_session_id:
            return
        session = self.store.load(self.selected_session_id)
        if session is None or session.status not in {"created", "running"}:
            return
        self.manager.cancel(self.selected_session_id)

    def action_refresh(self) -> None:
        self.refresh_sessions()
        self.refresh_artifacts()

    def action_clear_log(self) -> None:
        self._model_buffers.clear()
        self._recent_model_stream_text.clear()
        self.query_one("#transcript", RichLog).clear()
        self._last_transcript_event_type = None

    def action_scroll_transcript_up(self) -> None:
        self._scroll_transcript(lines=-4)

    def action_scroll_transcript_down(self) -> None:
        self._scroll_transcript(lines=4)

    def action_scroll_transcript_page_up(self) -> None:
        self._scroll_transcript(page=-1)

    def action_scroll_transcript_page_down(self) -> None:
        self._scroll_transcript(page=1)

    def _scroll_transcript(self, *, lines: int = 0, page: int = 0) -> None:
        try:
            transcript = self.query_one("#transcript", RichLog)
        except Exception:
            return
        transcript.auto_scroll = False
        if page < 0:
            transcript.scroll_page_up(animate=False, force=True)
        elif page > 0:
            transcript.scroll_page_down(animate=False, force=True)
        elif lines:
            transcript.scroll_relative(y=lines, animate=False, force=True, immediate=True)
        self._refocus_composer_soon()

    def action_open_artifact(self) -> None:
        self._open_artifact(self.selected_artifact_path)

    def action_toggle_inspector(self) -> None:
        if self._home_mode and not self.selected_session_id:
            self._inspector_visible = False
            self._sync_sidebar_visibility()
            self._update_statusbar()
            return
        self._inspector_visible = not self._inspector_visible
        if self._inspector_visible and self.selected_session_id:
            self._set_home_mode(False)
        self._sync_sidebar_visibility()
        self.refresh_artifacts()
        self._update_session_detail()
        self._update_statusbar()
        self.query_one("#command", ComposerInput).focus()

    def action_toggle_tool_details(self) -> None:
        self._tool_details_visible = not self._tool_details_visible
        if self.selected_session_id:
            self._load_session_log(self.selected_session_id)
            self.refresh_artifacts()
        else:
            self._update_statusbar()
        self.query_one("#command", ComposerInput).focus()

    def action_focus_artifacts(self) -> None:
        if not self.selected_session_id:
            self.action_show_sessions()
            return
        self._inspector_visible = True
        self._set_home_mode(False)
        self._sync_sidebar_visibility()
        self.refresh_artifacts()
        self._update_session_detail()
        self._update_statusbar()
        self.query_one("#command", ComposerInput).focus()

    def action_show_shortcuts(self) -> None:
        if isinstance(self.screen, ModalScreen):
            return

        def selected(command: Optional[str]) -> None:
            command_input = self.query_one("#command", ComposerInput)
            if command:
                self._handle_command(f"/{command}")
            command_input.focus()

        self.push_screen(
            CommandPalette(
                SHORTCUT_PALETTE,
                title="Keyboard",
                placeholder="Search shortcuts",
            ),
            selected,
        )

    def action_show_commands(self) -> None:
        if isinstance(self.screen, ModalScreen):
            handler = getattr(self.screen, "action_cursor_up", None)
            if handler is not None:
                handler()
            return

        def selected(command: Optional[str]) -> None:
            if command is None:
                self.query_one("#command", ComposerInput).focus()
                return
            command_input = self.query_one("#command", ComposerInput)
            if command.endswith(" "):
                self._set_composer_text(command)
                command_input.focus()
                return
            self._handle_command(command)
            command_input.focus()

        self.push_screen(CommandPalette(COMMAND_PALETTE), selected)

    def action_show_browser_modes(self) -> None:
        if isinstance(self.screen, ModalScreen):
            return

        def selected(command: Optional[str]) -> None:
            if command:
                self._handle_command(command)
            self.query_one("#command", ComposerInput).focus()

        self.push_screen(
            CommandPalette(
                BROWSER_MODE_PALETTE,
                title="Browser modes",
                placeholder="Choose browser backend",
            ),
            selected,
        )

    def action_show_model_selector(self) -> None:
        provider = self.provider_label or "provider"
        self._open_command_selector(f"Select {provider} model", f"Search {provider} models", self._model_palette())

    def action_show_provider_selector(self) -> None:
        self._open_command_selector("Select provider", "Search providers", PROVIDER_PALETTE)

    def action_show_settings(self) -> None:
        self._open_command_selector("Settings", "Search settings", self._settings_palette())

    def action_show_auth(self) -> None:
        self._open_command_selector("Authentication", "Search auth actions", self._auth_palette())

    def _open_command_selector(
        self,
        title: str,
        placeholder: str,
        commands: list[tuple[str, str, str]],
    ) -> None:
        if isinstance(self.screen, ModalScreen):
            return

        def selected(command: Optional[str]) -> None:
            command_input = self.query_one("#command", ComposerInput)
            if command:
                if command.endswith(" "):
                    self._set_composer_text(f"/{command}")
                    command_input.focus()
                    return
                self._handle_command(f"/{command}")
            command_input.focus()

        self.push_screen(CommandPalette(commands, title=title, placeholder=placeholder), selected)

    def action_show_sessions(self) -> None:
        if isinstance(self.screen, ModalScreen):
            return

        def selected(session_id: Optional[str]) -> None:
            if not session_id:
                self.query_one("#command", ComposerInput).focus()
                return
            self.selected_session_id = session_id
            self._load_session_log(session_id)
            self.refresh_artifacts()
            self._update_session_detail()
            self.query_one("#command", ComposerInput).focus()

        self.push_screen(SessionPalette(self._session_rows()), selected)

    def _start_task(self, task: str) -> None:
        session = self.manager.start(task)
        self.selected_session_id = session.id
        self.selected_artifact_path = None
        self._load_session_log(session.id)
        self.refresh_sessions()
        self.refresh_artifacts()

    def _resume_session(self, session_id: str, instruction: str) -> None:
        log = self.query_one("#transcript", RichLog)
        follow_transcript = self._should_follow_transcript_updates(log)
        session = self.store.load(session_id)
        if session is None:
            log.write(f"[#e4e4e7]session not found: {escape(session_id)}[/]")
            return
        try:
            resumed = self.manager.resume(session.id, instruction)
        except Exception as exc:
            log.write(f"[#e4e4e7]resume failed: {escape(str(exc))}[/]")
            return
        self.selected_session_id = resumed.id
        self.selected_artifact_path = None
        self._load_session_log(resumed.id, follow=follow_transcript)
        self.refresh_sessions()
        self.refresh_artifacts()

    def _persist_config_values(self, values: dict[str, Any]) -> Optional[Path]:
        for dotted, value in values.items():
            _assign_config_value(self.config, dotted, value)
        apply_config_environment(self.config, override=True)
        if self.config_path is None:
            return None
        try:
            path, _ = write_config_values(values, path=self.config_path)
        except Exception as exc:
            self.query_one("#transcript", RichLog).write(f"[#e4e4e7]config save failed: {escape(str(exc))}[/]")
            return None
        return path

    def _saved_suffix(self, path: Optional[Path]) -> str:
        if path is None:
            return ""
        return f" [#7a7d86]saved {escape(str(path))}[/]"

    def _set_browser_mode(self, mode: str) -> None:
        log = self.query_one("#transcript", RichLog)
        normalized = _normalize_browser_mode(mode)
        if normalized is None:
            log.write(f"[#e4e4e7]unknown browser mode: {escape(mode)}[/]")
            log.write("[#7a7d86]expected auto, chromium, real, remote, cdp, or daemon[/]")
            return
        os.environ["LLM_BROWSER_MODE"] = normalized
        values: dict[str, Any] = {"browser.mode": normalized}
        if normalized == "headless-chromium":
            os.environ["LLM_BROWSER_HEADLESS"] = "1"
            values["browser.headless"] = True
        elif normalized == "real":
            os.environ["LLM_BROWSER_HEADLESS"] = "0"
            values["browser.headless"] = False
        path = self._persist_config_values(values)
        log.write(f"[#e4e4e7]browser[/] {escape(_browser_mode_label(normalized))}{self._saved_suffix(path)}")
        self._update_statusbar()
        self._update_session_detail()

    def _set_provider(self, provider: str) -> None:
        log = self.query_one("#transcript", RichLog)
        normalized = provider.strip().lower()
        if normalized not in provider_names():
            log.write(f"[#e4e4e7]unknown provider: {escape(provider)}[/]")
            log.write(f"[#7a7d86]expected one of: {escape(', '.join(provider_names()))}[/]")
            return
        self.provider_label = normalized
        values: dict[str, Any] = {"provider": normalized}
        default_model = default_model_for_provider(normalized)
        if default_model and self.model_label != default_model:
            self.model_label = default_model
            os.environ["LLM_BROWSER_MODEL"] = default_model
            values["model"] = default_model
        path = self._persist_config_values(values)
        suffix = f" · model {self.model_label}" if self.model_label else ""
        log.write(f"[#e4e4e7]provider[/] {escape(normalized)}{escape(suffix)}{self._saved_suffix(path)}")
        self._update_statusbar()
        self._update_session_detail()

    def _set_model(self, model: str) -> None:
        log = self.query_one("#transcript", RichLog)
        model = model.strip()
        if not model:
            log.write("[#e4e4e7]model requires a model id[/]")
            return
        self.model_label = model
        os.environ["LLM_BROWSER_MODEL"] = model
        if self.provider_label == "codex":
            os.environ["LLM_BROWSER_CODEX_MODEL"] = model
        path = self._persist_config_values({"model": model})
        log.write(f"[#e4e4e7]model[/] {escape(model)}{self._saved_suffix(path)}")
        self._update_statusbar()
        self._update_session_detail()

    def _set_bool_env(self, env_name: str, value: str, label: str, config_key: Optional[str] = None) -> bool:
        log = self.query_one("#transcript", RichLog)
        normalized = value.strip().lower()
        if normalized in {"1", "true", "yes", "on"}:
            os.environ[env_name] = "1"
            path = self._persist_config_values({config_key: True}) if config_key else None
            log.write(f"[#e4e4e7]{escape(label)}[/] on{self._saved_suffix(path)}")
            self._update_statusbar()
            self._update_session_detail()
            return True
        if normalized in {"0", "false", "no", "off"}:
            os.environ[env_name] = "0"
            path = self._persist_config_values({config_key: False}) if config_key else None
            log.write(f"[#e4e4e7]{escape(label)}[/] off{self._saved_suffix(path)}")
            self._update_statusbar()
            self._update_session_detail()
            return True
        log.write(f"[#e4e4e7]{escape(label)} expects on or off[/]")
        return False

    def _set_viewport(self, values: list[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        if not values:
            width = os.environ.get("LLM_BROWSER_WIDTH") or "1280"
            height = os.environ.get("LLM_BROWSER_HEIGHT") or "900"
            log.write(f"[#7a7d86]viewport: {escape(width)}x{escape(height)}[/]")
            return
        if len(values) == 1 and "x" in values[0].lower():
            width, height = values[0].lower().split("x", 1)
        elif len(values) >= 2:
            width, height = values[0], values[1]
        else:
            log.write("[#e4e4e7]viewport expects WIDTH HEIGHT or WIDTHxHEIGHT[/]")
            return
        try:
            width_i = int(width)
            height_i = int(height)
        except ValueError:
            log.write("[#e4e4e7]viewport width and height must be integers[/]")
            return
        if width_i <= 0 or height_i <= 0:
            log.write("[#e4e4e7]viewport width and height must be positive[/]")
            return
        os.environ["LLM_BROWSER_WIDTH"] = str(width_i)
        os.environ["LLM_BROWSER_HEIGHT"] = str(height_i)
        path = self._persist_config_values({"browser.width": width_i, "browser.height": height_i})
        log.write(f"[#e4e4e7]viewport[/] {width_i}x{height_i}{self._saved_suffix(path)}")
        self._update_statusbar()
        self._update_session_detail()

    def _set_max_turns(self, value: str) -> None:
        log = self.query_one("#transcript", RichLog)
        try:
            max_turns = int(value)
        except ValueError:
            log.write("[#e4e4e7]max-turns expects an integer[/]")
            return
        if max_turns <= 0:
            log.write("[#e4e4e7]max-turns must be positive[/]")
            return
        self.manager.max_turns = max_turns
        path = self._persist_config_values({"max_turns": max_turns})
        log.write(f"[#e4e4e7]max turns[/] {max_turns}{self._saved_suffix(path)}")
        self._update_session_detail()

    def _set_config_value(self, args: list[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        if len(args) < 2:
            log.write("[#e4e4e7]set expects a setting name and value[/]")
            return
        key = args[0].strip().lower().replace("_", "-")
        value = " ".join(args[1:]).strip()
        env_map = {
            "cdp-url": ("LLM_BROWSER_CDP_HTTP_URL", "browser.cdp_url", False),
            "cdp-ws": ("LLM_BROWSER_CDP_WS_URL", "browser.cdp_ws", False),
            "chrome-path": ("LLM_BROWSER_CHROME_PATH", "browser.chrome_path", False),
            "profile-template": ("LLM_BROWSER_PROFILE_TEMPLATE", "browser.profile_template", False),
            "browser-use-api-key": ("BROWSER_USE_API_KEY", "browser.cloud_api_key", True),
            "cloud-api-key": ("BROWSER_USE_API_KEY", "browser.cloud_api_key", True),
            "remote-api-key": ("BROWSER_USE_API_KEY", "browser.cloud_api_key", True),
            "cloud-api-base": ("LLM_BROWSER_CLOUD_API_BASE", "browser.cloud_api_base", False),
            "cloud-profile-id": ("LLM_BROWSER_CLOUD_PROFILE_ID", "browser.cloud_profile_id", False),
            "cloud-profile-name": ("LLM_BROWSER_CLOUD_PROFILE_NAME", "browser.cloud_profile_name", False),
            "cloud-proxy-country": ("LLM_BROWSER_CLOUD_PROXY_COUNTRY", "browser.cloud_proxy_country", False),
            "cloud-timeout": ("LLM_BROWSER_CLOUD_TIMEOUT", "browser.cloud_timeout", False),
            "cloud-custom-proxy-json": ("LLM_BROWSER_CLOUD_CUSTOM_PROXY_JSON", "browser.cloud_custom_proxy_json", True),
            "openai-api-key": ("LLM_BROWSER_OPENAI_API_KEY", "openai.api_key", True),
            "openai-base-url": ("LLM_BROWSER_OPENAI_BASE_URL", "openai.base_url", False),
            "codex-base-url": ("LLM_BROWSER_CODEX_BASE_URL", "codex.base_url", False),
            "daemon-name": ("LLM_BROWSER_DAEMON_NAME", "browser.daemon_name", False),
            "daemon-backend": ("LLM_BROWSER_DAEMON_BACKEND", "browser.daemon_backend", False),
        }
        bool_env_map = {
            "headless": ("LLM_BROWSER_HEADLESS", "headless", "browser.headless"),
            "keep-profile": ("LLM_BROWSER_KEEP_CHROME_PROFILE", "keep profile", "browser.keep_profile"),
            "cloud-allow-resizing": ("LLM_BROWSER_CLOUD_ALLOW_RESIZING", "cloud resizing", "browser.cloud_allow_resizing"),
            "cloud-recording": ("LLM_BROWSER_CLOUD_ENABLE_RECORDING", "cloud recording", "browser.cloud_recording"),
        }
        if key in {"model"}:
            self._set_model(value)
            return
        if key in {"provider"}:
            self._set_provider(value)
            return
        if key in {"browser", "browser-mode"}:
            self._set_browser_mode(value)
            return
        if key in {"viewport"}:
            self._set_viewport(value.split())
            return
        if key in {"max-turns", "maxturns"}:
            self._set_max_turns(value)
            return
        if key in bool_env_map:
            env_name, label, config_key = bool_env_map[key]
            self._set_bool_env(env_name, value, label, config_key)
            return
        setting = env_map.get(key)
        if setting is None:
            log.write(f"[#e4e4e7]unknown setting: {escape(key)}[/]")
            return
        env_name, config_key, sensitive = setting
        os.environ[env_name] = value
        parsed_value: Any = value
        if key == "cloud-timeout":
            try:
                parsed_value = int(value)
            except ValueError:
                log.write("[#e4e4e7]cloud-timeout expects an integer[/]")
                return
        elif key == "cloud-custom-proxy-json":
            try:
                json.loads(value)
            except ValueError as exc:
                log.write(f"[#e4e4e7]cloud-custom-proxy-json expects a JSON object: {escape(str(exc))}[/]")
                return
        path = self._persist_config_values({config_key: parsed_value})
        shown = "<redacted>" if sensitive and value else value
        log.write(f"[#e4e4e7]{escape(key)}[/] {escape(shown)}{self._saved_suffix(path)}")
        self._update_statusbar()
        self._update_session_detail()

    def _handle_auth_command(self, args: list[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        if not args:
            self.action_show_auth()
            return

        subcommand = args[0].strip().lower()
        rest = args[1:]
        if subcommand in {"status", "show"}:
            self._write_auth_status()
            return
        if subcommand in {"browser-use", "browseruse", "cloud", "remote"}:
            if rest:
                self._save_browser_use_api_key(" ".join(rest))
            else:
                self._prompt_browser_use_api_key()
            return
        if subcommand in {"api-key", "apikey", "key"}:
            if not rest:
                self.action_show_auth()
                return
            provider = _normalize_auth_provider(rest[0])
            if provider is None:
                log.write(f"[#e4e4e7]unknown API-key provider: {escape(rest[0])}[/]")
                return
            if len(rest) > 1:
                self._save_provider_api_key(provider, " ".join(rest[1:]))
            else:
                self._prompt_provider_api_key(provider)
            return
        if subcommand == "remove":
            if not rest:
                log.write("[#e4e4e7]auth remove expects a provider[/]")
                return
            provider = _normalize_auth_provider(rest[0])
            if provider is None:
                log.write(f"[#e4e4e7]unknown provider: {escape(rest[0])}[/]")
                return
            self._remove_provider_auth(provider)
            return
        if subcommand == "codex":
            self._handle_codex_auth_command(rest)
            return

        provider = _normalize_auth_provider(subcommand)
        if provider is None:
            log.write(f"[#e4e4e7]unknown auth command: {escape(subcommand)}[/]")
            log.write("[#7a7d86]try /auth, /auth status, /auth api-key openrouter, or /auth anthropic login[/]")
            return
        if rest and rest[0].strip().lower() in {"remove", "logout"}:
            self._remove_provider_auth(provider)
            return
        if provider == "anthropic" and rest and rest[0].strip().lower() in {"login", "oauth", "claude-code", "claudecode"}:
            self._start_anthropic_oauth_login()
            return
        if rest and rest[0].strip().lower() in {"api-key", "apikey", "key"}:
            if len(rest) > 1:
                self._save_provider_api_key(provider, " ".join(rest[1:]))
            else:
                self._prompt_provider_api_key(provider)
            return
        if rest:
            self._save_provider_api_key(provider, " ".join(rest))
        else:
            self._prompt_provider_api_key(provider)

    def _handle_codex_auth_command(self, args: list[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        action = args[0].strip().lower() if args else "status"
        if action == "status":
            self._write_auth_status()
            return
        if action == "login":
            self._start_codex_login()
            return
        if action == "import":
            try:
                from llm_browser.auth import import_codex_cli_auth

                auth = import_codex_cli_auth()
            except Exception as exc:
                log.write(f"[#e4e4e7]Codex import failed: {escape(str(exc))}[/]")
                return
            log.write(f"[#e4e4e7]Codex auth imported[/] [#7a7d86]{escape(str(auth.source_path))}[/]")
            return
        if action in {"logout", "remove"}:
            from llm_browser.auth import logout_codex_auth

            removed = logout_codex_auth()
            log.write(f"[#e4e4e7]Codex auth removed[/] [#7a7d86]{'yes' if removed else 'no stored auth'}[/]")
            return
        log.write(f"[#e4e4e7]unknown Codex auth command: {escape(action)}[/]")

    def _prompt_provider_api_key(self, provider: str) -> None:
        label = _auth_provider_label(provider)
        self._pending_secret = ("provider", provider)

        def selected(value: Optional[str]) -> None:
            self._pending_secret = None
            if value:
                self._save_provider_api_key(provider, value)
            self.query_one("#command", ComposerInput).focus()

        self.push_screen(
            SecretInputScreen(
                f"{label} API key",
                f"Paste a {label} API key. It will be stored in the local provider auth store.",
                placeholder=f"{label} API key",
            ),
            selected,
        )

    def _prompt_browser_use_api_key(self) -> None:
        self._pending_secret = ("browser-use", "")

        def selected(value: Optional[str]) -> None:
            self._pending_secret = None
            if value:
                self._save_browser_use_api_key(value)
            self.query_one("#command", ComposerInput).focus()

        self.push_screen(
            SecretInputScreen(
                "Browser Use API key",
                "Paste a Browser Use cloud API key. It will be saved redacted in config output.",
                placeholder="Browser Use API key",
            ),
            selected,
        )

    def _consume_pending_secret(self, value: str) -> bool:
        pending = self._pending_secret
        if pending is None:
            return False
        self._pending_secret = None
        kind, provider = pending
        if isinstance(self.screen, SecretInputScreen):
            self.screen.dismiss(None)
        if kind == "provider":
            self._save_provider_api_key(provider, value)
        elif kind == "browser-use":
            self._save_browser_use_api_key(value)
        else:
            self.query_one("#transcript", RichLog).write(f"[#e4e4e7]unknown secret prompt: {escape(kind)}[/]")
        return True

    def _save_browser_use_api_key(self, key: str) -> None:
        self._set_config_value(["browser-use-api-key", key])

    def _save_provider_api_key(self, provider: str, key: str) -> None:
        log = self.query_one("#transcript", RichLog)
        key = key.strip()
        if not key:
            log.write("[#e4e4e7]API key cannot be empty[/]")
            return
        if provider not in API_KEY_PROVIDERS:
            log.write(f"[#e4e4e7]provider does not use API keys here: {escape(provider)}[/]")
            return
        from llm_browser.auth.store import ProviderAuthStore

        ProviderAuthStore().set_api_key(provider, key)
        env_name = _auth_provider_env_name(provider)
        if env_name:
            os.environ[env_name] = key
        log.write(f"[#e4e4e7]{escape(_auth_provider_label(provider))} API key[/] <redacted> [#7a7d86]saved[/]")
        self._update_statusbar()
        self._update_session_detail()

    def _remove_provider_auth(self, provider: str) -> None:
        from llm_browser.auth.store import ProviderAuthStore

        removed = ProviderAuthStore().remove(provider)
        env_name = _auth_provider_env_name(provider)
        if env_name:
            os.environ.pop(env_name, None)
        self.query_one("#transcript", RichLog).write(
            f"[#e4e4e7]{escape(_auth_provider_label(provider))} auth removed[/] [#7a7d86]{'yes' if removed else 'no stored auth'}[/]"
        )

    def _write_auth_status(self) -> None:
        from llm_browser.auth import auth_status
        from llm_browser.auth.store import provider_auth_status

        browser_use_env = os.environ.get("BROWSER_USE_API_KEY") or os.environ.get("BU_API_KEY")
        payload = {
            "codex": auth_status(),
            "providers": provider_auth_status(),
            "browser_use": {"available": bool(browser_use_env), "source": "environment" if browser_use_env else "missing"},
        }
        self.query_one("#transcript", RichLog).write(escape(json.dumps(payload, indent=2)))

    def _start_codex_login(self) -> None:
        log = self.query_one("#transcript", RichLog)
        log.write("[#9b9ca5]starting Codex login...[/]")

        def run() -> None:
            try:
                import webbrowser

                from llm_browser.auth import complete_device_code_login, request_device_code

                device = request_device_code()
                webbrowser.open(device.verification_url)
                self.call_from_thread(
                    self._write_auth_line,
                    (
                        "Codex login opened in your browser.\n"
                        f"URL: {device.verification_url}\n"
                        f"Code: {device.user_code}\n"
                        "Waiting for authorization..."
                    ),
                )
                auth = complete_device_code_login(device)
                self.call_from_thread(self._write_auth_line, f"Codex login complete. Account {auth.account_id}")
            except Exception as exc:
                self.call_from_thread(self._write_auth_line, f"Codex login failed: {exc}", error=True)

        threading.Thread(target=run, name="browser-use-terminal-codex-login", daemon=True).start()

    def _start_anthropic_oauth_login(self) -> None:
        log = self.query_one("#transcript", RichLog)
        log.write("[#9b9ca5]starting Anthropic Claude Code login...[/]")

        def run() -> None:
            try:
                from llm_browser.auth.anthropic import login_anthropic_oauth
                from llm_browser.auth.store import ProviderAuthStore

                credentials = login_anthropic_oauth(
                    open_browser=True,
                    timeout_s=900,
                    on_url=lambda url: self.call_from_thread(
                        self._write_auth_line,
                        f"Anthropic login opened in your browser.\nURL: {url}",
                    ),
                    on_status=lambda text: self.call_from_thread(self._write_auth_line, text),
                    prompt_on_timeout=False,
                )
                ProviderAuthStore().set_oauth(
                    "anthropic",
                    access=str(credentials["access"]),
                    refresh=str(credentials["refresh"]),
                    expires=int(credentials["expires"]),
                )
                self.call_from_thread(self._write_auth_line, "Anthropic Claude Code login complete.")
            except Exception as exc:
                self.call_from_thread(self._write_auth_line, f"Anthropic login failed: {exc}", error=True)

        threading.Thread(target=run, name="browser-use-terminal-anthropic-login", daemon=True).start()

    def _write_auth_line(self, text: str, *, error: bool = False) -> None:
        style = "#e4e4e7" if error else "#9b9ca5"
        self.query_one("#transcript", RichLog).write(f"[{style}]{escape(text)}[/]")

    def _model_palette(self) -> list[tuple[str, str, str]]:
        current = self.model_label
        rows = []
        if current:
            rows.append(("Current", f"model {current}", f"Current {self.provider_label} model"))
        seen = {current} if current else set()
        for spec in models_for_provider(self.provider_label):
            model = spec.model
            if model and model in seen:
                continue
            rows.append((spec.display_name, f"model {model}", spec.description))
            seen.add(model)
        rows.append(("Change provider", "provider", "Choose a provider first, then pick one of its models"))
        rows.append(("Custom", "model ", "Type a custom model id for the current provider"))
        return rows

    def _settings_palette(self) -> list[tuple[str, str, str]]:
        width = os.environ.get("LLM_BROWSER_WIDTH") or "1280"
        height = os.environ.get("LLM_BROWSER_HEIGHT") or "900"
        current_auth_provider = _normalize_auth_provider(self.provider_label)
        current_auth_command = f"auth api-key {current_auth_provider}" if current_auth_provider else "auth"
        current_auth_detail = (
            f"Set key for {_auth_provider_label(current_auth_provider)}"
            if current_auth_provider
            else "Open auth actions for this provider"
        )
        return [
            ("Provider", "provider", f"Current {self.provider_label}"),
            ("Model", "model", f"Current {self.model_label or '-'}"),
            ("Authentication", "auth", "Manage provider API keys and logins"),
            ("Current provider auth", current_auth_command, current_auth_detail),
            ("Browser", "browser", f"Current {_browser_runtime_label()}"),
            ("Browser Use API key", "auth browser-use ", "Paste and save remote browser API key"),
            ("OpenRouter API key", "auth api-key openrouter", "Prompt and save OpenRouter API key"),
            ("OpenAI API key", "auth api-key openai", "Prompt and save OpenAI API key"),
            ("Anthropic API key", "auth api-key anthropic", "Prompt and save Anthropic API key"),
            ("Claude Code login", "auth anthropic login", "Login to Anthropic through Claude Code OAuth"),
            ("Z.ai API key", "auth api-key zai", "Prompt and save Z.ai API key"),
            ("Qwen API key", "auth api-key qwen", "Prompt and save Qwen API key"),
            ("Headless on", "headless on", "Run owned Chromium headless"),
            ("Headless off", "headless off", "Show owned Chromium window"),
            ("Viewport", "viewport ", f"Current {width}x{height}"),
            ("Max turns", "max-turns ", f"Current {self.manager.max_turns}"),
            ("CDP URL", "set cdp-url ", "Attach to DevTools HTTP endpoint"),
            ("CDP websocket", "set cdp-ws ", "Attach to raw DevTools websocket"),
            ("Chrome path", "set chrome-path ", "Custom Chrome/Chromium executable"),
            ("Profile template", "set profile-template ", "Copy an existing profile template"),
            ("Keep profile on", "set keep-profile on", "Preserve owned Chromium profile"),
            ("Keep profile off", "set keep-profile off", "Delete owned Chromium profile on close"),
            ("Cloud API base", "set cloud-api-base ", "Custom Browser Use API base URL"),
            ("Cloud profile id", "set cloud-profile-id ", "Browser Use cloud profile UUID"),
            ("Cloud profile name", "set cloud-profile-name ", "Browser Use cloud profile name"),
            ("Cloud proxy country", "set cloud-proxy-country ", "Browser Use proxy country"),
            ("Cloud timeout", "set cloud-timeout ", "Browser Use timeout in minutes"),
            ("Cloud recording on", "set cloud-recording on", "Enable Browser Use cloud recording"),
            ("Cloud recording off", "set cloud-recording off", "Disable Browser Use cloud recording"),
            ("Cloud resizing on", "set cloud-allow-resizing on", "Allow cloud browser resizing"),
            ("Cloud resizing off", "set cloud-allow-resizing off", "Disable cloud browser resizing"),
            ("OpenAI base URL", "set openai-base-url ", "Custom OpenAI-compatible API base"),
            ("Codex base URL", "set codex-base-url ", "Custom Codex backend API base"),
            ("Daemon name", "set daemon-name ", "Daemon browser name"),
            ("Daemon backend", "set daemon-backend ", "Daemon backend: chromium, cdp, real, cloud"),
            ("Config", "config", "Show redacted loaded config"),
        ]

    def _auth_palette(self) -> list[tuple[str, str, str]]:
        rows: list[tuple[str, str, str]] = [
            ("Auth status", "auth status", "Show redacted auth status"),
            ("Codex login", "auth codex login", "Open browser device login for Codex subscription auth"),
            ("Import Codex CLI auth", "auth codex import", "Copy existing Codex CLI auth into this app"),
            ("Anthropic Claude Code login", "auth anthropic login", "Open browser OAuth login for Claude Code"),
            ("Browser Use API key", "auth browser-use", "Prompt and save Browser Use cloud API key"),
        ]
        current_provider = _normalize_auth_provider(self.provider_label)
        if current_provider is not None:
            rows.append(
                (
                    "Current provider API key",
                    f"auth api-key {current_provider}",
                    f"Prompt and save {_auth_provider_label(current_provider)} API key",
                )
            )
            rows.append(
                (
                    "Remove current provider auth",
                    f"auth remove {current_provider}",
                    f"Remove stored {_auth_provider_label(current_provider)} API key or OAuth token",
                )
            )
        for provider in API_KEY_PROVIDERS:
            label = _auth_provider_label(provider)
            rows.append((f"{label} API key", f"auth api-key {provider}", f"Prompt and save {label} API key"))
        return rows

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
            log.write("[#e4e4e7]no selected artifact[/]")
            return
        artifact = Path(path).expanduser()
        if not artifact.exists():
            log.write(f"[#e4e4e7]artifact not found: {escape(str(artifact))}[/]")
            return
        try:
            subprocess.Popen(["open", str(artifact)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            log.write(f"[#e4e4e7]opened {escape(str(artifact))}[/]")
        except Exception as exc:
            log.write(f"[#9b9ca5]open failed: {escape(str(exc))}; path: {escape(str(artifact))}[/]")

    def _write_trace(self, session_id: Optional[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        if not session_id:
            log.write("[#e4e4e7]no selected session for trace[/]")
            return
        from llm_browser.session.trace import write_trace_bundle

        try:
            path = write_trace_bundle(self.store, session_id)
        except Exception as exc:
            log.write(f"[#e4e4e7]trace failed: {escape(str(exc))}[/]")
            return
        self.selected_artifact_path = str(path)
        log.write(f"[#e4e4e7]trace written: {escape(str(path))}[/]")
        self.refresh_artifacts()

    def _start_self_eval(self, session_id: Optional[str]) -> None:
        log = self.query_one("#transcript", RichLog)
        if not session_id:
            log.write("[#e4e4e7]no selected session for eval[/]")
            return
        from llm_browser.session.trace import build_self_eval_prompt

        try:
            prompt = build_self_eval_prompt(self.store, session_id)
        except Exception as exc:
            log.write(f"[#e4e4e7]eval prompt failed: {escape(str(exc))}[/]")
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
            log.write(f"[#e4e4e7]report failed: {escape(str(exc))}[/]")
            return

        failed = _short_task_list(summary["failed_task_ids"])
        pending = _short_task_list(summary["pending_task_ids"])
        log.write(
            "[bold #e4e4e7]dataset report[/bold #e4e4e7] "
            f"{escape(str(summary['run_id']))}  "
            f"{escape(str(summary['dataset']))}  "
            f"passed [#e4e4e7]{summary['passed']}[/] / {summary['selected']}  "
            f"failed [#e4e4e7]{summary['failed']}[/]  "
            f"pending [#9b9ca5]{summary['pending']}[/]"
        )
        log.write(f"[#e4e4e7]failed:[/] {escape(failed)}")
        log.write(f"[#9b9ca5]pending:[/] {escape(pending)}")

    def _update_statusbar(self) -> None:
        meta = (
            f"[#e4e4e7]Build[/] [#6f727d]·[/] "
            f"[#e4e4e7]{escape(self.model_label or '-')}[/] "
            f"[#9b9ca5]{escape(self.provider_label)}  {escape(_browser_runtime_label())}[/]"
        )
        run_summary = self._selected_run_summary_text()
        if run_summary:
            meta += f"  {run_summary}"
        self.query_one("#composer-meta", Static).update(meta)
        self.query_one("#runtime-meta", Static).update(
            f"{escape(self.model_label or '-')}  ·  {escape(self.provider_label)}  ·  {escape(_browser_runtime_label())}"
        )

        selected_running = False
        if self.selected_session_id:
            selected = self.store.load(self.selected_session_id)
            selected_running = bool(selected and selected.status in {"created", "running"})
        if time.monotonic() < self._quit_hint_until:
            hint_items = [("ctrl+c", "again to quit", True)]
        else:
            if selected_running:
                hint_items = [("esc", "interrupt", True)]
            else:
                hint_items = [("tab", "sessions", True)]
            if not self._home_mode:
                hint_items.append(("f2", "hide inspector" if self._inspector_visible else "inspector", False))
                if self._inspector_visible:
                    hint_items.append(("f3", "artifacts", False))
            hint_items.extend(
                [
                    ("/", "settings", False),
                    ("ctrl+p", "commands", False),
                    ("f1", "keys", False),
                    ("ctrl+q", "quit", False),
                ]
            )
        hintbar = self.query_one("#hintbar", Static)
        hint_width = int(hintbar.size.width or self.query_one("#workspace").size.width or self.size.width or 80)
        hintbar.update(_footer_hintbar_markup(hint_items, width=hint_width))

        cwd = "-"
        if self.selected_session_id:
            session = self.store.load(self.selected_session_id)
            if session is not None:
                cwd = _compact_path(session.cwd)
        self.query_one("#sidebar-footer", Static).update(
            f"[#9b9ca5]{escape(cwd)}[/]\n[#e4e4e7]•[/] [bold #9b9ca5]{PRODUCT_NAME}[/bold #9b9ca5]"
        )

    def _update_activity_strip(self) -> None:
        activity = self.query_one("#activity", Static)
        if self._home_mode or not self.selected_session_id:
            activity.display = False
            activity.update("")
            return
        session = self.store.load(self.selected_session_id)
        if session is None or session.status not in {"created", "running"}:
            activity.display = False
            activity.update("")
            return

        self._activity_frame += 1
        frame = _ACTIVITY_FRAMES[self._activity_frame % len(_ACTIVITY_FRAMES)]
        events = self.store.events.read(session.id)
        live_url = _latest_browser_live_url(events)
        current_tool = _current_tool(events)
        if live_url:
            activity.update(
                f"[#9b9ca5]{frame} browsing · [/]"
                f"{_rich_link(_compact_live_preview_url(live_url), live_url)}"
            )
        else:
            detail = ""
            if current_tool != "-":
                detail = f" · {escape(_compact_inline(current_tool, limit=110))}"
            activity.update(f"[#9b9ca5]{frame} browsing{detail}[/]")
        activity.display = True

    def _update_session_detail(self) -> None:
        detail = self.query_one("#session-detail", Static)
        session_id = self.selected_session_id
        if not session_id:
            detail.update(
                "[#9b9ca5]❯[/] [bold #e4e4e7]New task[/bold #e4e4e7]\n\n"
                "[#9b9ca5]not started[/]\n"
                "[#9b9ca5]press tab for history[/]\n\n"
                "[bold #e4e4e7]browser[/bold #e4e4e7]\n"
                f"[#9b9ca5]{escape(_browser_runtime_label())}[/]"
            )
            return
        session = self.store.load(session_id)
        if session is None:
            detail.update(f"Missing session: {escape(session_id)}")
            return
        events = self.store.events.read(session.id)
        task = self._task_for_session(session)
        current_tool = _current_tool(events)
        final_line = _final_line(events)
        latest_image = _latest_image_line(events)
        live_url = _latest_browser_live_url(events)
        title = _compact_inline(task or "Browser session", limit=42)
        result_lines = []
        if latest_image != "-":
            result_lines.append(_sidebar_image_line(latest_image))
        if final_line != "-" and session.status in {"done", "failed", "cancelled"}:
            result_lines.extend(_sidebar_result_lines(final_line))
        if current_tool != "-" and session.status in {"created", "running"} and not current_tool.endswith(" done"):
            result_lines.append(f"[#9b9ca5]working: {escape(_compact_inline(current_tool, limit=46))}[/]")
        if not result_lines:
            waiting = "browsing live preview" if session.status in {"created", "running"} and live_url else "idle"
            if session.status in {"created", "running"} and not live_url:
                waiting = "waiting for browser output"
            result_lines.append(f"[#9b9ca5]{waiting}[/]")
        result_markup = "\n".join(result_lines[:4])
        browser_markup = ""
        if session.status in {"created", "running"} and live_url:
            browser_markup = (
                "[bold #e4e4e7]browser[/bold #e4e4e7]\n"
                f"{_rich_link('open live preview', live_url)}\n"
                f"{_rich_link(_compact_inline(live_url, limit=42), live_url)}\n\n"
            )
        usage_markup = _sidebar_usage_markup(events)
        if usage_markup:
            usage_markup += "\n\n"
        detail.update(
            f"[#9b9ca5]❯[/] [bold #e4e4e7]{escape(title)}[/bold #e4e4e7]\n\n"
            f"{_status_markup(session.status)} [#9b9ca5]· {escape(session.id)}[/]\n"
            f"[#9b9ca5]updated {_format_age(session.updated_ms / 1000)}[/]\n\n"
            f"{browser_markup}"
            f"{usage_markup}"
            f"[bold #e4e4e7]answer[/bold #e4e4e7]\n"
            f"{result_markup}"
        )

    def _preview_artifact(self, path: Optional[str], force: bool = False) -> None:
        preview = self.query_one("#artifact-preview", RichLog)
        if not path:
            self._preview_key = None
            preview.clear()
            preview.write("[#7a7d86]No artifact selected.[/]")
            return
        artifact = Path(path)
        if not artifact.exists():
            self._preview_key = None
            preview.clear()
            preview.write(f"[#e4e4e7]Missing artifact: {escape(str(artifact))}[/]")
            return
        stat = artifact.stat()
        key = (str(artifact), stat.st_mtime, stat.st_size)
        if not force and key == self._preview_key:
            return
        self._preview_key = key
        preview.clear()
        kind = _artifact_kind(artifact)
        display_name = artifact.name
        if self.selected_session_id:
            session = self.store.load(self.selected_session_id)
            if session is not None:
                display_name = _artifact_display_name(session, artifact)
        preview.write(f"[bold #e4e4e7]{escape(display_name)}[/bold #e4e4e7]")
        preview.write(f"[#7a7d86]{kind} · {_format_bytes(stat.st_size)}[/]")
        preview.write(_rich_link(f"open {kind}", artifact.resolve().as_uri()))
        preview.write("")
        if kind == "image":
            dims = _image_dimensions(artifact)
            if dims:
                preview.write(f"[#7a7d86]dimensions: {dims[0]} x {dims[1]}[/]")
            meta = artifact.with_suffix(".json")
            if meta.exists():
                try:
                    meta_payload = json.loads(meta.read_text(encoding="utf-8"))
                    preview.write(_screenshot_meta_summary(meta_payload))
                except Exception:
                    pass
            preview.scroll_home(animate=False)
            return
        if artifact.suffix.lower() == ".md":
            try:
                text, mode = _preview_text_for_artifact(artifact)
                if mode == "markdown":
                    _write_markdown(preview, text)
                else:
                    preview.write(escape(text))
            except Exception as exc:
                preview.write(f"[#9b9ca5]preview failed: {escape(str(exc))}[/]")
            preview.scroll_home(animate=False)
            return
        if artifact.suffix.lower() in {".txt", ".json", ".jsonl", ".html", ".csv", ".tsv", ".py"}:
            try:
                text, mode = _preview_text_for_artifact(artifact)
                if mode == "markdown":
                    _write_markdown(preview, text)
                else:
                    preview.write(escape(text))
            except Exception as exc:
                preview.write(f"[#9b9ca5]preview failed: {escape(str(exc))}[/]")
            preview.scroll_home(animate=False)
            return
        preview.write("[#7a7d86]Binary artifact. Press `o` or `/open` to view it.[/]")
        preview.scroll_home(animate=False)

    def _task_for_session(self, session: SessionMetadata) -> str:
        for event in reversed(self.store.events.read(session.id)):
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
            return f"[#7a7d86]run[/] {escape(run_id)}"
        label = _dataset_run_label_from_id(run_id)
        return (
            f"[#7a7d86]run[/] [#e4e4e7]{escape(label)}[/] "
            f"[#e4e4e7]{summary['passed']}/{summary['selected']} done[/] "
            f"[#9b9ca5]{summary['pending']} pending[/]"
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
            if "chrome-profile" in parts or "__pycache__" in parts or "compactions" in parts:
                continue
            if _is_sidecar_metadata(path):
                continue
            artifact_paths.append(path)
    paths.extend(artifact_paths)
    state_dir = session.state_dir.resolve()
    cwd = session.cwd.resolve()
    if cwd.exists() and cwd != session.artifact_dir.resolve() and state_dir in cwd.parents:
        paths.extend([path for path in cwd.rglob("*") if path.is_file()])
    return sorted(set(paths), key=_artifact_sort_key)[:200]


def _is_sidecar_metadata(path: Path) -> bool:
    if path.suffix.lower() != ".json":
        return False
    companion = path.with_suffix(".png")
    if companion.exists() and "screenshots" in path.parts:
        return True
    return False


def _artifact_sort_key(path: Path) -> tuple[int, float, str]:
    priority = {
        "md": 0,
        "download": 1,
        "image": 2,
        "tool": 4,
        "workspace": 6,
        "py": 7,
        "trace": 5,
        "json": 8,
    }.get(_artifact_kind(path), 6)
    return (priority, -path.stat().st_mtime, str(path))


def _artifact_display_name(session: SessionMetadata, path: Path) -> str:
    try:
        relative = path.relative_to(session.artifact_dir)
    except ValueError:
        try:
            relative = path.relative_to(session.cwd)
        except ValueError:
            relative = Path(path.name)
    if "tool-output" in relative.parts:
        tool = path.stem.split("_")[-1] or "tool"
        return _compact_inline(f"{tool} output", limit=27)
    if "screenshots" in relative.parts:
        return _compact_inline(path.name, limit=27)
    return _compact_inline(str(relative), limit=27)


def _screenshot_meta_summary(payload: object) -> str:
    if not isinstance(payload, dict):
        return ""
    lines: list[str] = []
    url = str(payload.get("url") or "").strip()
    title = str(payload.get("title") or "").strip()
    viewport = payload.get("viewport")
    if title:
        lines.append(f"[#9b9ca5]title: {escape(_compact_inline(title, limit=36))}[/]")
    if url:
        lines.append(_rich_link(_compact_inline(url, limit=42), url))
    if isinstance(viewport, dict):
        width = viewport.get("width")
        height = viewport.get("height")
        if width and height:
            lines.append(f"[#9b9ca5]viewport: {width} x {height}[/]")
    return "\n".join(lines)


def _browser_headless_default() -> bool:
    value = os.environ.get("LLM_BROWSER_HEADLESS")
    if value is None:
        return True
    return value.lower() in {"1", "true", "yes", "on"}


def _normalize_browser_mode(mode: str) -> Optional[str]:
    normalized = mode.strip().lower().replace("_", "-")
    aliases = {
        "": "auto",
        "auto": "auto",
        "chromium": "chromium",
        "chrome": "chromium",
        "local": "chromium",
        "owned": "chromium",
        "headless": "headless-chromium",
        "headless-chromium": "headless-chromium",
        "real": "real",
        "real-chrome": "real",
        "existing": "real",
        "remote": "cloud",
        "cloud": "cloud",
        "browser-use": "cloud",
        "browser-use-cloud": "cloud",
        "cdp": "cdp",
        "attach": "cdp",
        "daemon": "daemon",
    }
    return aliases.get(normalized)


def _normalize_auth_provider(provider: str) -> Optional[str]:
    normalized = provider.strip().lower().replace("_", "-")
    return AUTH_PROVIDER_ALIASES.get(normalized)


def _auth_provider_label(provider: str) -> str:
    return AUTH_PROVIDER_LABELS.get(provider, provider)


def _auth_provider_env_name(provider: str) -> str:
    return {
        "openai": "LLM_BROWSER_OPENAI_API_KEY",
        "anthropic": "LLM_BROWSER_ANTHROPIC_API_KEY",
        "zai": "LLM_BROWSER_ZAI_API_KEY",
        "qwen": "LLM_BROWSER_QWEN_API_KEY",
        "openrouter": "LLM_BROWSER_OPENROUTER_API_KEY",
    }.get(provider, "")


def _browser_mode_label(mode: str) -> str:
    labels = {
        "auto": "auto",
        "chromium": "chromium",
        "headless-chromium": "chromium headless",
        "real": "real chrome",
        "cloud": "remote browser-use",
        "cdp": "cdp",
        "daemon": "daemon",
    }
    return labels.get(mode, mode)


def _browser_runtime_label() -> str:
    diagnostics = browser_runtime_diagnostics()
    mode = str(diagnostics.get("mode") or "auto")
    if mode == "headless-chromium":
        return "chromium headless"
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


def _sidebar_usage_markup(events: list[Event]) -> str:
    usage = summarize_usage_events(events)
    if usage.entries <= 0:
        return ""

    cost_label = f"{format_cost(usage.total_cost_usd)} est" if usage.has_cost else "cost unavailable"
    input_total = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens
    generated = usage.output_tokens + usage.reasoning_tokens
    lines = [
        "[bold #e4e4e7]usage[/bold #e4e4e7]",
        f"[#e4e4e7]{escape(cost_label)}[/] [#9b9ca5]· {escape(format_tokens(usage.total_tokens))} tokens[/]",
        f"[#9b9ca5]in {escape(format_tokens(input_total))} · out {escape(format_tokens(generated))}[/]",
    ]
    if usage.reasoning_tokens:
        lines.append(f"[#9b9ca5]reasoning {escape(format_tokens(usage.reasoning_tokens))}[/]")
    cache_total = usage.cache_read_tokens + usage.cache_write_tokens
    if cache_total:
        lines.append(
            f"[#9b9ca5]cache read {escape(format_tokens(usage.cache_read_tokens))}"
            f" · write {escape(format_tokens(usage.cache_write_tokens))}[/]"
        )
    if usage.latest_usage is not None:
        latest_model = _compact_inline(usage.latest_model or "unknown", limit=34)
        lines.append(
            f"[#9b9ca5]latest {escape(format_tokens(usage.latest_usage.context_tokens))}"
            f" · {escape(latest_model)}[/]"
        )
    return "\n".join(lines)


def _format_event_for_transcript(event: Event) -> str:
    payload = event.payload
    if event.type == "session.created":
        return ""
    if event.type == "session.input":
        return _summarize_task_text(str(payload.get("text") or ""))
    if event.type == "session.followup":
        return _summarize_task_text(str(payload.get("text") or payload.get("instruction") or ""))
    if event.type == "session.status":
        return ""
    if event.type == "session.cancel_requested":
        return f"cancel requested: {payload.get('reason', '')}"
    if event.type == "session.compaction_started":
        return f"compacting: {payload.get('reason', '')} {payload.get('phase', '')}".strip()
    if event.type == "session.compacted":
        return f"compacted: before={payload.get('before_messages')} after={payload.get('after_messages')}"
    if event.type == "session.compaction_failed":
        return f"compact failed: {_compact_error_text(payload.get('error') or '')}"
    if event.type == "session.deadline_warning":
        return f"deadline warning: {payload.get('remaining_s')}s remaining"
    if event.type == "model.usage":
        return ""
    if event.type == "browser.live_url":
        url = str(payload.get("live_url") or payload.get("url") or "").strip()
        if not url:
            return _browser_action_line("live", "preview")
        return _browser_action_line("live", _compact_live_preview_url(url))
    if event.type == "tool.started":
        name = str(payload.get("name") or "tool")
        return _format_tool_started_summary(name, payload.get("arguments") or {})
    if event.type == "tool.image":
        image = payload.get("image") or {}
        path = Path(str(image.get("path") or ""))
        label = image.get("label") or "image"
        return f"image: {label} -> {path.name or path}"
    if event.type == "tool.output":
        text = str(payload.get("text") or "")
        return _format_tool_output_for_transcript(
            str(payload.get("name") or "tool"),
            str(payload.get("stream") or ""),
            text,
        )
    if event.type == "tool.finished":
        name = str(payload.get("name") or "tool")
        if name == "done":
            return ""
        return _format_tool_finished_for_transcript(name, payload.get("output") or {})
    if event.type == "tool.failed":
        return f"tool failed: {payload.get('name') or 'tool'} {_compact_error_text(payload.get('error') or '')}".strip()
    if event.type == "session.done":
        return str(payload.get("result") or "") or "done"
    if event.type == "session.failed":
        return f"failed: {_compact_error_text(payload.get('error') or '')}".strip()
    return f"{event.type}: {payload}"


def _transcript_event_type(event: Event) -> str:
    if event.type == "session.input" and event.payload.get("resumed"):
        return "session.followup"
    return event.type


def _format_default_transcript_event(event: Event) -> list[tuple[str, str]]:
    if event.type == "tool.started":
        return _format_default_tool_started_event(event)
    if event.type == "tool.output":
        return []
    if event.type == "tool.finished":
        name = str(event.payload.get("name") or "tool")
        if name == "done":
            return []
        line = _format_tool_finished_for_transcript(name, event.payload.get("output") or {})
        return [(line, "tool.finished")] if line and line.startswith("✕") else []
    line = _format_event_for_transcript(event)
    if not line:
        return []
    event_type = "browser.action" if line.startswith(_BROWSER_ACTION_PREFIX) else _transcript_event_type(event)
    return [(line, event_type)]


def _format_default_tool_started_event(event: Event) -> list[tuple[str, str]]:
    name = str(event.payload.get("name") or "tool")
    arguments = event.payload.get("arguments") or {}
    if isinstance(arguments, dict) and name == "python":
        code = str(arguments.get("code") or "").strip()
        browser_lines = _browser_action_lines_for_code(code)
        if browser_lines:
            return [(line, "browser.action") for line in browser_lines]
        return []
    line = _format_tool_started_summary(name, arguments)
    if line.startswith(_BROWSER_ACTION_PREFIX):
        return [(line, "browser.action")]
    return []


def _browser_action_lines_for_code(code: str) -> list[str]:
    if not code:
        return []
    tree = _parse_python_code(code)
    if tree is None:
        return []
    actions = _browser_actions_from_tree(tree)
    if not actions:
        return []
    visible_actions = actions[:6]
    lines = [_browser_action_line(*action) for action in visible_actions]
    hidden_count = len(actions) - len(visible_actions)
    if hidden_count > 0:
        verbs = ", ".join(action[0] for action in actions[len(visible_actions) : len(visible_actions) + 4])
        if len(actions) > len(visible_actions) + 4:
            verbs += f", +{len(actions) - len(visible_actions) - 4}"
        lines.append(_browser_action_line("steps", f"+{hidden_count} more browser moves", verbs))
    return lines


def _format_events_for_transcript(events: list[Event], *, show_tools: bool = False) -> list[tuple[str, str]]:
    lines: list[tuple[str, str]] = []
    model_buffers: dict[str, str] = {}
    last_by_session: dict[str, str] = {}
    recent_model_by_session: dict[str, str] = {}
    streamed_model_by_session: dict[str, str] = {}
    streamed_tool_output: dict[tuple[str, str], str] = {}

    def flush(session_id: str) -> None:
        text = model_buffers.pop(session_id, "")
        if text.strip():
            rendered = text.strip()
            lines.append((rendered, "model.delta"))
            last_by_session[session_id] = _canonical_transcript_text(rendered)
            recent_model_by_session[session_id] = _join_transcript_text(
                recent_model_by_session.get(session_id, ""),
                rendered,
            )

    for event in events:
        if event.type == "model.delta":
            text = str(event.payload.get("text") or "")
            if not text:
                continue
            streamed_model_by_session[event.session_id] = (
                streamed_model_by_session.get(event.session_id, "") + text
            )[-12000:]
            buffered = model_buffers.get(event.session_id, "") + text
            model_buffers[event.session_id] = buffered
            if "\n" in buffered or len(buffered) >= 700:
                flush(event.session_id)
            continue
        flush(event.session_id)
        if show_tools and event.type == "tool.finished":
            call_id = str(event.payload.get("tool_call_id") or "")
            output = event.payload.get("output") or {}
            text = str(output.get("text") or "") if isinstance(output, dict) else ""
            repeats_stream = bool(
                call_id
                and text.strip()
                and _canonical_transcript_text(streamed_tool_output.get((event.session_id, call_id), ""))
                == _canonical_transcript_text(text)
            )
            line = _format_tool_finished_for_transcript(
                str(event.payload.get("name") or "tool"),
                output,
                omit_text=repeats_stream,
            )
            formatted_items = [(line, _transcript_event_type(event))] if line else []
        else:
            if show_tools:
                line = _format_event_for_transcript(event)
                formatted_items = [(line, _transcript_event_type(event))] if line else []
            else:
                formatted_items = _format_default_transcript_event(event)
        for line, event_type in formatted_items:
            if not line:
                continue
            if event.type == "session.done":
                result = str(event.payload.get("result") or "")
                previous = last_by_session.get(event.session_id, "")
                recent_model = recent_model_by_session.get(event.session_id, "")
                streamed_model = streamed_model_by_session.get(event.session_id, "")
                canonical_result = _canonical_transcript_text(result)
                if (
                    previous == canonical_result
                    or previous == _canonical_transcript_text(line)
                    or _canonical_transcript_text(recent_model) == canonical_result
                    or _canonical_transcript_text(recent_model).endswith(canonical_result)
                    or _canonical_transcript_text(streamed_model) == canonical_result
                    or _canonical_transcript_text(streamed_model).endswith(canonical_result)
                ):
                    continue
            lines.append((line, event_type))
            last_by_session[event.session_id] = _canonical_transcript_text(line)
            if event.type == "tool.output":
                call_id = str(event.payload.get("tool_call_id") or "")
                raw_text = str(event.payload.get("text") or "")
                if call_id and raw_text.strip():
                    key = (event.session_id, call_id)
                    streamed_tool_output[key] = _join_transcript_text(streamed_tool_output.get(key, ""), raw_text)

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


def _write_markdown(log: RichLog, text: str, style: str = "none") -> None:
    if _file_links_for_text(text):
        log.write(_rich_text_with_file_links(text, style=style))
        return
    if not _looks_like_markdown(text):
        plain = _strip_inline_code_backticks(text)
        if _BARE_URL_RE.search(plain):
            log.write(_rich_text_with_file_links(plain, style=style))
        else:
            base_style = None if style == "none" else style
            log.write(Text(plain, style=base_style))
        return
    log.write(
        Markdown(
            _linkify_markdown(text),
            code_theme="monokai",
            hyperlinks=True,
            inline_code_theme="monokai",
            style=style,
        )
    )


def _write_transcript_markdown(log: RichLog, text: str, style: str = "none") -> None:
    if _needs_block_markdown(text):
        _write_markdown(log, text, style=style)
        return
    lines = _compact_transcript_markdown_lines(text)
    for line in lines:
        if not line:
            log.write("")
            continue
        log.write(_compact_markdown_line_text(line, style=style))


def _needs_block_markdown(text: str) -> bool:
    return (
        "```" in text
        or "~~~" in text
        or re.search(r"(?m)^\|.+\|$", text) is not None
    )


def _compact_transcript_markdown_lines(text: str) -> list[str]:
    source_lines = str(text or "").splitlines()
    if not source_lines:
        return []
    lines: list[str] = []
    for index, line in enumerate(source_lines):
        stripped = line.rstrip()
        if stripped:
            lines.append(stripped)
            continue
        previous = _nearest_nonempty_line(source_lines, index, -1)
        next_line = _nearest_nonempty_line(source_lines, index, 1)
        if previous and next_line and _is_markdown_list_line(previous) and _is_markdown_list_line(next_line):
            continue
        if not lines or lines[-1] == "":
            continue
        lines.append("")
    return lines


def _nearest_nonempty_line(lines: list[str], index: int, step: int) -> str:
    cursor = index + step
    while 0 <= cursor < len(lines):
        if lines[cursor].strip():
            return lines[cursor]
        cursor += step
    return ""


def _is_markdown_list_line(line: str) -> bool:
    return bool(_UNORDERED_LIST_LINE_RE.match(line) or _ORDERED_LIST_LINE_RE.match(line))


def _compact_markdown_line_text(line: str, style: str = "none") -> Text:
    base_style = None if style == "none" else style
    heading_match = _HEADING_LINE_RE.match(line)
    if heading_match:
        rendered = Text(style=base_style)
        _append_compact_inline(rendered, heading_match.group(1).strip(), style=_inline_style(style, bold=True))
        return rendered

    unordered_match = _UNORDERED_LIST_LINE_RE.match(line)
    if unordered_match:
        rendered = Text(style=base_style)
        rendered.append(unordered_match.group(1))
        rendered.append("• ", style="#9b9ca5")
        _append_compact_inline(rendered, unordered_match.group(3), style=style)
        return rendered

    ordered_match = _ORDERED_LIST_LINE_RE.match(line)
    if ordered_match:
        rendered = Text(style=base_style)
        rendered.append(ordered_match.group(1))
        rendered.append(f"{ordered_match.group(2).rjust(2)} ", style="bold #55d7ee")
        _append_compact_inline(rendered, ordered_match.group(3), style=style)
        return rendered

    rendered = Text(style=base_style)
    _append_compact_inline(rendered, line, style=style)
    return rendered


def _append_compact_inline(rendered: Text, text: str, style: str = "none") -> None:
    cursor = 0
    for match in _COMPACT_INLINE_MARKDOWN_RE.finditer(text):
        if match.start() < cursor:
            continue
        if match.start() > cursor:
            _append_linkified_segment(rendered, text[cursor : match.start()], style=style)
        trailing_space_count = len(text[match.end() :]) - len(text[match.end() :].lstrip(" "))
        trailing_spaces = "\u00a0" * trailing_space_count
        if match.group("bold") is not None:
            _append_linkified_segment(
                rendered,
                match.group("bold") + trailing_spaces,
                style=_inline_style(style, bold=True),
            )
        elif match.group("code") is not None:
            _append_linkified_segment(rendered, match.group("code") + trailing_spaces, style="#d7a84f")
        else:
            target = (match.group("target") or "").strip()
            if target.startswith("<") and target.endswith(">"):
                target = target[1:-1]
            rendered.append((match.group("label") or target) + trailing_spaces, style=f"#8aa6a0 underline link {target}")
        cursor = match.end() + trailing_space_count
    if cursor < len(text):
        _append_linkified_segment(rendered, text[cursor:], style=style)


def _append_linkified_segment(rendered: Text, text: str, style: str = "none") -> None:
    if not text:
        return
    base_style = None if style == "none" else style
    leading_space_count = len(text) - len(text.lstrip(" "))
    if leading_space_count and rendered.plain:
        rendered.append("\u00a0" * leading_space_count, style=base_style)
        text = text[leading_space_count:]
        if not text:
            return
    cursor = 0
    for start, end, label, target in _link_spans_for_text(text):
        if start < cursor:
            continue
        if start > cursor:
            rendered.append(text[cursor:start], style=base_style)
        rendered.append(label, style=f"#8aa6a0 underline link {target}")
        cursor = end
    if cursor < len(text):
        rendered.append(text[cursor:], style=base_style)


def _inline_style(style: str, *, bold: bool = False) -> str:
    color = "#e4e4e7" if style == "none" else style
    return f"bold {color}" if bold else color


def _write_tool_started_line(log: RichLog, line: str) -> None:
    if not line.startswith(_BROWSER_ACTION_PREFIX):
        log.write(f"[#9b9ca5]{escape(line)}[/]")
        return
    _write_browser_action_line(log, line)


def _write_browser_action_line(log: RichLog, line: str) -> None:
    if not line.startswith(_BROWSER_ACTION_PREFIX):
        _write_markdown(log, line)
        return
    body, _, note = line.removeprefix(_BROWSER_ACTION_PREFIX).partition(" · ")
    verb, _, detail = body.partition(" ")
    detail = detail.strip()
    row = Text("  ")
    verb = verb or "work"
    action_style = _BROWSER_ACTION_STYLES.get(verb, "#5c9cf5")
    icon = _BROWSER_ACTION_ICONS.get(verb, "┃")
    row.append(f"{icon} ", style=f"bold {action_style}")
    row.append(f"{verb:<6}", style=f"bold {action_style}")
    if detail:
        row.append("  ")
        row.append(detail, style="#e4e4e7")
    if note:
        row.append("  ·  ", style="#555963")
        row.append(note, style="#8f929c")
    log.write(row)


def _fenced_json(value: object) -> str:
    try:
        rendered = json.dumps(value, ensure_ascii=False, indent=2, default=str)
    except TypeError:
        rendered = repr(value)
    return f"```json\n{rendered}\n```"


def _format_tool_output_for_transcript(name: str, stream: str, text: str) -> str:
    snippet = _tool_transcript_snippet(text, max_lines=2, max_chars=240)
    if not snippet.strip():
        return ""
    header = f"{name} {stream}".strip()
    return f"{header} · {snippet}".strip()


def _format_tool_finished_for_transcript(name: str, output: object, *, omit_text: bool = False) -> str:
    if not isinstance(output, dict):
        return f"✓ {_tool_display_name(name)}"
    text = str(output.get("text") or "")
    data = {
        key: value
        for key, value in output.items()
        if key not in {"text", "images"} and not _is_empty_tool_value(value)
    }
    if _is_success_metadata(data):
        data = {}
    image_count = len(output.get("images") or [])
    if (not text.strip() or omit_text) and not data and not image_count:
        return ""
    data_summary = _tool_data_summary(data)
    if data_summary:
        data = {}
    icon = "✕" if data_summary.startswith("exit ") and not data_summary.endswith(" 0") else "✓"
    parts: list[str] = []
    display_name = _tool_display_name(name)
    if text.strip() and not omit_text:
        line = f"{icon} {display_name} · {_tool_transcript_snippet(text, max_lines=2, max_chars=320)}"
        if data_summary:
            line += f" · {data_summary}"
        parts.append(line)
    else:
        parts.append(f"{icon} {display_name}" + (f" · {data_summary}" if data_summary else ""))
    if data:
        parts.append(_fenced_json(data))
    if image_count and not omit_text:
        label = "image" if image_count == 1 else "images"
        parts.append(f"{image_count} {label} attached")
    return "\n\n".join(parts)


def _tool_display_name(name: str) -> str:
    return "result" if name == "python" else name


def _format_tool_started_summary(name: str, arguments: object) -> str:
    if not isinstance(arguments, dict):
        return f"→ {name}"
    if name == "shell":
        command = str(arguments.get("command") or "").strip()
        if command:
            return f"→ shell {_summarize_command(command)}"
    if name == "python":
        code = str(arguments.get("code") or "").strip()
        if code:
            explicit_summary = _explicit_code_summary(code)
            if explicit_summary:
                return f"→ {explicit_summary}"
            browser_summary = _browser_action_summary(code)
            if browser_summary:
                return browser_summary
            return f"→ python {_summarize_code(code)}"
    keys = ", ".join(str(key) for key in list(arguments)[:4])
    return f"→ {name}" + (f" {keys}" if keys else "")


def _summarize_command(command: str) -> str:
    first_line = next((line.strip() for line in command.splitlines() if line.strip()), "")
    if "cat >" in first_line and "<<'" in first_line:
        target = first_line.split("cat >", 1)[1].split("<<", 1)[0].strip()
        return f"write {target}"
    if first_line.startswith("pwd") and ("ls" in first_line or "find" in first_line):
        return "inspect workspace"
    if "README.md" in first_line and "sed -n" in first_line:
        return "read README.md"
    if "pyproject.toml" in first_line and ("cat" in first_line or "sed -n" in first_line):
        return "read pyproject.toml"
    if first_line.startswith("find src"):
        if "-name '*.py'" in first_line or '-name "*.py"' in first_line:
            return "list python sources"
        return "list source files"
    sed_paths = re.findall(r"sed -n ['\"][^'\"]+['\"]\\?\s*([^;&|]+)", first_line)
    sed_paths = [path.strip() for path in sed_paths if path.strip()]
    if sed_paths:
        compact_paths = ", ".join(_compact_path(Path(path), limit=34) for path in sed_paths[:2])
        if len(sed_paths) > 2:
            compact_paths += f" +{len(sed_paths) - 2}"
        return f"read {compact_paths}"
    if first_line.startswith("zed "):
        return f"open {first_line.removeprefix('zed ').strip()}"
    if first_line.startswith("open "):
        return first_line
    return _compact_inline(first_line, limit=96)


def _summarize_code(code: str) -> str:
    explicit_summary = _explicit_code_summary(code)
    if explicit_summary:
        return explicit_summary
    meaningful_lines = [
        line.strip()
        for line in code.splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]
    if not meaningful_lines:
        return "run code"
    line_prefix = f"{len(meaningful_lines)} lines · " if len(meaningful_lines) > 1 else ""
    tree = _parse_python_code(code)
    intent = _python_code_intent(code, meaningful_lines, tree)
    return f"{line_prefix}{intent}" if intent else f"{line_prefix}{_compact_inline(meaningful_lines[0], limit=96)}"


def _explicit_code_summary(code: str) -> str:
    for line in code.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        match = re.match(r"#\s*but\s*:\s*(.+)$", stripped, flags=re.IGNORECASE)
        if not match:
            return ""
        return _compact_inline(match.group(1).strip(), limit=120)
    return ""


def _browser_action_summary(code: str) -> str:
    tree = _parse_python_code(code)
    if tree is None:
        return ""
    actions = _browser_actions_from_tree(tree)
    if not actions:
        helper_imports = _browser_helper_imports(tree)
        if helper_imports and tree.body and all(isinstance(node, (ast.Import, ast.ImportFrom)) for node in tree.body):
            return _browser_action_line("kit", "browser helpers", ", ".join(_browser_helper_roles(helper_imports)))
        return ""
    if len(actions) == 1:
        return _browser_action_line(*actions[0])
    verbs = ", ".join(action[0] for action in actions[:4])
    if len(actions) > 4:
        verbs += f", +{len(actions) - 4}"
    return _browser_action_line("steps", f"{len(actions)} browser moves", verbs)


def _browser_action_line(verb: str, detail: str = "", note: str = "") -> str:
    line = f"{_BROWSER_ACTION_PREFIX}{verb}"
    if detail:
        line += f" {_compact_inline(detail, limit=88)}"
    if note:
        line += f" · {_compact_inline(note, limit=88)}"
    return line


def _browser_actions_from_tree(tree: ast.Module) -> list[tuple[str, str, str]]:
    calls = sorted(
        (node for node in ast.walk(tree) if isinstance(node, ast.Call)),
        key=lambda node: (getattr(node, "lineno", 0), getattr(node, "col_offset", 0)),
    )
    actions: list[tuple[str, str, str]] = []
    for call in calls:
        action = _browser_action_from_call(call)
        if action is not None:
            actions.append(action)
    return actions


def _browser_action_from_call(call: ast.Call) -> Optional[tuple[str, str, str]]:
    name = _call_name(call.func)
    if name in {"goto_url", "navigate", "open_url", "new_tab"}:
        target = _browser_target_label(_call_string_arg(call, 0))
        return ("open", target or "new tab", "")
    if name in {"wait_for_load", "wait_for_page_load"}:
        return ("wait", "page load", "")
    if name == "wait_for_network_idle":
        return ("wait", "network idle", _timeout_note(call))
    if name in {"wait_for_element", "wait_for_selector"}:
        selector = _call_string_arg(call, 0) or "element"
        note = "visible" if _call_bool_kw(call, "visible") else _timeout_note(call)
        return ("watch", selector, note)
    if name == "wait_for_text":
        return ("watch", _quoted_preview(_call_string_arg(call, 0) or "text"), _timeout_note(call))
    if name in {"capture_screenshot", "screenshot"}:
        label = _call_string_arg(call, 0) or _call_string_kw(call, "label") or "page"
        return ("snap", f"{label} screenshot", "")
    if name == "screenshot_element":
        selector = _call_string_arg(call, 0) or "element"
        return ("snap", selector, "element")
    if name == "click_text":
        return ("tap", _quoted_preview(_call_string_arg(call, 0) or "text"), "")
    if name in {"click_at_xy", "click_at", "coordinate_click"}:
        x = _call_literal_arg(call, 0)
        y = _call_literal_arg(call, 1)
        detail = f"{x}, {y}" if x is not None and y is not None else "coordinates"
        return ("tap", detail, "")
    if name == "fill_input":
        selector = _call_string_arg(call, 0) or "input"
        typed = _call_string_arg(call, 1)
        note = f"{len(typed)} chars" if typed else ""
        return ("type", selector, note)
    if name == "type_text":
        typed = _call_string_arg(call, 0)
        return ("type", _quoted_preview(typed or "text"), f"{len(typed)} chars" if typed else "")
    if name in {"press_key", "press"}:
        key = _call_string_arg(call, 0) or "key"
        return ("key", key, "")
    if name == "scroll":
        return ("scroll", _scroll_action_detail(call), "")
    if name in {"js", "evaluate"}:
        return ("peek", _browser_js_action_detail(_call_string_arg(call, 0)), "")
    if name == "http_get":
        target = _browser_target_label(_call_string_arg(call, 0))
        return ("fetch", target or "page", "")
    if name == "load_skill":
        return ("kit", _call_string_arg(call, 0) or "browser skill", "")
    if name in {"list_tabs", "current_tab", "switch_tab", "ensure_real_tab"}:
        return ("tabs", name.replace("_", " "), "")
    return None


def _parse_python_code(code: str) -> Optional[ast.Module]:
    try:
        return ast.parse(code)
    except SyntaxError:
        return None


def _python_code_intent(code: str, lines: list[str], tree: Optional[ast.Module]) -> str:
    lower = code.lower()
    if tree and tree.body and all(isinstance(node, (ast.Import, ast.ImportFrom)) for node in tree.body):
        browser_helpers = _browser_helper_imports(tree)
        if browser_helpers:
            return f"prepare browser helpers: {', '.join(_browser_helper_roles(browser_helpers))}"
        imports = _import_names(tree)
        if imports:
            return f"prepare imports: {', '.join(imports[:6])}" + (f" +{len(imports) - 6}" if len(imports) > 6 else "")
    if tree:
        url = _first_string_call_arg(tree, {"goto_url", "new_tab", "navigate", "open_url"})
        if url:
            return f"open {_compact_inline(url, limit=84)}"
        if _has_call(tree, {"capture_screenshot", "screenshot"}):
            return "capture screenshot"
        if _has_call(tree, {"wait_for_load", "wait_for_page_load"}):
            return "wait for page load"
        if _has_call(tree, {"js", "evaluate"}):
            return _summarize_browser_js(tree)
    writes_json = any(token in lower for token in ("json.dump", "json.dumps", ".write_text", "open("))
    if "country" in lower and ("price" in lower or "pricing" in lower):
        return "extract country pricing" + (" to JSON" if writes_json else "")
    if "beautifulsoup" in lower or "bs4" in lower:
        return "scrape page HTML with requests/bs4"
    if "requests." in lower or "requests " in lower:
        return "fetch page HTML"
    if "globals()" in lower or "locals()" in lower:
        return "inspect Python workspace"
    if tree and _has_call(tree, {"print"}):
        return f"inspect output: {_compact_inline(lines[0], limit=84)}"
    if any("'''" in line or '"""' in line for line in lines) and len(lines) <= 3:
        return "prepare extraction script"
    return _compact_inline(lines[0], limit=96)


def _import_names(tree: ast.Module) -> list[str]:
    names: list[str] = []
    for node in tree.body:
        if isinstance(node, ast.Import):
            names.extend(alias.name.split(".", 1)[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module:
            names.append(node.module.split(".", 1)[0])
    return list(dict.fromkeys(names))


def _browser_helper_imports(tree: ast.Module) -> list[str]:
    names: list[str] = []
    for node in tree.body:
        if isinstance(node, ast.ImportFrom) and node.module in {"browser", "browser_helpers"}:
            names.extend(alias.name for alias in node.names)
    return list(dict.fromkeys(names))


def _browser_helper_roles(names: list[str]) -> list[str]:
    roles: list[str] = []
    helper_names = set(names)
    if helper_names & {"goto_url", "new_tab", "navigate", "open_url"}:
        roles.append("nav")
    if helper_names & {"wait_for_load", "wait_for_page_load"}:
        roles.append("wait")
    if helper_names & {"capture_screenshot", "screenshot"}:
        roles.append("screenshot")
    if helper_names & {"js", "evaluate"}:
        roles.append("JS")
    roles.extend(name for name in names if name not in helper_names or not roles)
    return roles or names[:4]


def _has_call(tree: ast.Module, names: set[str]) -> bool:
    return any(_call_name(node.func) in names for node in ast.walk(tree) if isinstance(node, ast.Call))


def _first_string_call_arg(tree: ast.Module, names: set[str]) -> str:
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call) or _call_name(node.func) not in names or not node.args:
            continue
        first = node.args[0]
        if isinstance(first, ast.Constant) and isinstance(first.value, str):
            return first.value
    return ""


def _call_literal_arg(call: ast.Call, index: int) -> object:
    if index >= len(call.args):
        return None
    arg = call.args[index]
    if isinstance(arg, ast.Constant):
        return arg.value
    return None


def _call_literal_kw(call: ast.Call, name: str) -> object:
    for keyword in call.keywords:
        if keyword.arg != name:
            continue
        value = keyword.value
        if isinstance(value, ast.Constant):
            return value.value
    return None


def _call_string_arg(call: ast.Call, index: int) -> str:
    value = _call_literal_arg(call, index)
    return value if isinstance(value, str) else ""


def _call_string_kw(call: ast.Call, name: str) -> str:
    value = _call_literal_kw(call, name)
    return value if isinstance(value, str) else ""


def _call_bool_kw(call: ast.Call, name: str) -> bool:
    value = _call_literal_kw(call, name)
    return bool(value) if isinstance(value, bool) else False


def _timeout_note(call: ast.Call) -> str:
    value = _call_literal_kw(call, "timeout_s")
    if value is None:
        value = _call_literal_kw(call, "timeout")
    if isinstance(value, (int, float)):
        return f"{value:g}s"
    return ""


def _browser_target_label(url: str) -> str:
    if not url:
        return ""
    parsed = urlparse(url)
    if not parsed.netloc:
        return _compact_inline(url, limit=84)
    path = parsed.path.rstrip("/")
    label = parsed.netloc + (path if path and path != "/" else "")
    if parsed.query:
        label += "?" + parsed.query
    return _compact_inline(label, limit=84)


def _quoted_preview(value: str) -> str:
    if not value:
        return ""
    return f'"{_compact_inline(value, limit=48)}"'


def _scroll_action_detail(call: ast.Call) -> str:
    dy = _call_literal_kw(call, "dy")
    dx = _call_literal_kw(call, "dx")
    if isinstance(dy, (int, float)) and dy:
        return "down" if dy > 0 else "up"
    if isinstance(dx, (int, float)) and dx:
        return "right" if dx > 0 else "left"
    return "page"


def _browser_js_action_detail(script: str) -> str:
    script = script.lower()
    if "queryselector" in script or "queryselectorall" in script:
        return "DOM"
    if "document.documentelement.innerhtml" in script or "outerhtml" in script:
        return "page HTML"
    if "window." in script:
        return "app data"
    return "JavaScript"


def _summarize_browser_js(tree: ast.Module) -> str:
    script = _first_string_call_arg(tree, {"js", "evaluate"}).lower()
    if "queryselector" in script or "queryselectorall" in script:
        return "inspect page DOM"
    if "document.documentelement.innerhtml" in script or "outerhtml" in script:
        return "read page HTML"
    if "window." in script:
        return "read browser app data"
    return "run browser JavaScript"


def _call_name(func: ast.expr) -> str:
    if isinstance(func, ast.Name):
        return func.id
    if isinstance(func, ast.Attribute):
        return func.attr
    return ""


def _trim_tool_output_for_transcript(text: str, *, max_lines: int = 12, max_chars: int = 1200) -> str:
    stripped = text.strip()
    if len(stripped) <= max_chars and len(stripped.splitlines()) <= max_lines:
        return stripped
    lines = stripped.splitlines()[:max_lines]
    return "\n".join(lines).rstrip() + "\n..."


def _tool_transcript_snippet(text: str, *, max_lines: int, max_chars: int) -> str:
    stripped = text.strip()
    if not stripped:
        return ""
    lines = stripped.splitlines()
    if len(stripped) <= max_chars and len(lines) <= max_lines:
        return _compact_inline(" / ".join(line.strip() for line in lines if line.strip()), limit=max_chars)
    first = _first_meaningful_tool_line(lines)
    stats = _tool_text_stats(stripped)
    if first:
        return f"{stats} · {first}"
    return stats


def _first_meaningful_tool_line(lines: list[str]) -> str:
    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue
        if stripped in {"[", "{", "}", "]"}:
            continue
        return _compact_inline(stripped, limit=112)
    return ""


def _tool_text_stats(text: str) -> str:
    line_count = len(text.splitlines()) or 1
    return f"{line_count} lines · {_format_bytes(len(text.encode('utf-8')))}"


def _tool_data_summary(data: dict[str, object]) -> str:
    payload = data.get("data")
    if not isinstance(payload, dict):
        return ""
    returncode = payload.get("returncode")
    if returncode not in {None, 0, "0"}:
        return f"exit {returncode}"
    if payload.get("ok") is False:
        return "not ok"
    if payload.get("truncated") is True:
        return "truncated"
    return ""


def _format_tool_arguments_for_transcript(name: str, arguments: object) -> str:
    if not isinstance(arguments, dict):
        return _fenced_json(arguments)
    if name == "shell" and "command" in arguments:
        command = str(arguments.get("command") or "")
        extras = {key: value for key, value in arguments.items() if key != "command"}
        parts = [f"```sh\n{_truncate_multiline(command, max_lines=16, max_chars=1400)}\n```"]
        if extras:
            parts.append(_fenced_json(extras))
        return "\n\n".join(parts)
    if name == "python" and "code" in arguments:
        code = str(arguments.get("code") or "")
        extras = {key: value for key, value in arguments.items() if key != "code"}
        parts = [f"```python\n{_truncate_multiline(code, max_lines=18, max_chars=1600)}\n```"]
        if extras:
            parts.append(_fenced_json(extras))
        return "\n\n".join(parts)
    rendered = json.dumps(arguments, ensure_ascii=False, indent=2, default=str)
    if len(rendered) <= 1400:
        return f"```json\n{rendered}\n```"
    compact = {
        key: _compact_tool_argument_value(value)
        for key, value in arguments.items()
    }
    return _fenced_json(compact)


def _truncate_multiline(text: str, *, max_lines: int, max_chars: int) -> str:
    lines = text.splitlines()
    truncated = False
    if len(lines) > max_lines:
        lines = lines[:max_lines]
        truncated = True
    rendered = "\n".join(lines)
    if len(rendered) > max_chars:
        rendered = rendered[:max_chars].rstrip()
        truncated = True
    if truncated:
        hidden_lines = max(0, len(text.splitlines()) - len(lines))
        suffix = f"\n# ... truncated"
        if hidden_lines:
            suffix += f" ({hidden_lines} more lines)"
        rendered += suffix
    return rendered


def _compact_tool_argument_value(value: object) -> object:
    if isinstance(value, str):
        return _compact_inline(value, limit=260)
    if isinstance(value, list):
        return f"{len(value)} items"
    if isinstance(value, dict):
        return {key: _compact_tool_argument_value(item) for key, item in list(value.items())[:12]}
    return value


def _is_success_metadata(data: dict[str, object]) -> bool:
    if not data:
        return False
    payload = data.get("data")
    if not isinstance(payload, dict):
        return False
    allowed = {"returncode", "truncated", "ok"}
    if any(key not in allowed for key in payload):
        return False
    return payload.get("returncode", 0) == 0 and payload.get("truncated") in {False, None}


def _preview_text_for_artifact(path: Path, limit: int = 5000) -> tuple[str, str]:
    suffix = path.suffix.lower()
    raw = path.read_text(encoding="utf-8", errors="replace")
    raw = _strip_tool_output_metadata(raw)
    if suffix in {".json", ".jsonl"}:
        parsed = _parse_json_preview(raw, jsonl=suffix == ".jsonl")
        if parsed is not None:
            if isinstance(parsed, str):
                return (_limit_preview_text(parsed, limit), "plain")
            return (_fenced_json(parsed), "markdown")
    if "tool-output" in path.parts:
        parsed = _parse_json_preview(raw, jsonl=False)
        if parsed is not None:
            text = _best_text_from_preview_payload(parsed)
            if text:
                decoded = _decode_escaped_text(text)
                mode = "markdown" if _looks_like_markdown(decoded) else "plain"
                return (_limit_preview_text(decoded, limit), mode)
            return (_fenced_json(parsed), "markdown")
    text = _decode_escaped_text(raw)
    text = _limit_preview_text(text, limit)
    if suffix == ".md" or _looks_like_markdown(text):
        return (text, "markdown")
    return (text, "plain")


def _strip_tool_output_metadata(text: str) -> str:
    for separator in ("\n\ndata=", "\ndata="):
        if separator in text:
            return text.split(separator, 1)[0].rstrip()
    return text.rstrip()


def _parse_json_preview(text: str, *, jsonl: bool) -> object | None:
    stripped = text.strip()
    if not stripped:
        return ""
    if jsonl:
        rows = []
        for line in stripped.splitlines()[:80]:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except ValueError:
                return None
        return rows
    try:
        return json.loads(stripped)
    except ValueError:
        return None


def _best_text_from_preview_payload(value: object) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        strings = [item for item in value if isinstance(item, str) and item.strip()]
        if strings:
            unique = list(dict.fromkeys(strings))
            return max(unique, key=len)
    if isinstance(value, dict):
        for key in ("text", "result", "output"):
            item = value.get(key)
            if isinstance(item, str) and item.strip():
                return item
    return ""


def _decode_escaped_text(text: str) -> str:
    if not any(marker in text for marker in ("\\n", "\\u", "\\t", "\\r")):
        return text
    try:
        return text.encode("utf-8").decode("unicode_escape")
    except UnicodeError:
        return text


def _looks_like_markdown(text: str) -> bool:
    stripped = text.lstrip()
    return (
        stripped.startswith("#")
        or "\n#" in text
        or re.search(r"\[[^\]]+\]\([^)]+\)", text) is not None
        or "**" in text
        or re.search(r"(?m)^\s*[-*]\s+\S", text) is not None
        or re.search(r"(?m)^\|.+\|$", text) is not None
    )


def _limit_preview_text(text: str, limit: int) -> str:
    if len(text) <= limit:
        return text
    return text[:limit].rstrip() + "\n\n..."


def _is_empty_tool_value(value: object) -> bool:
    return value is None or value == "" or value == [] or value == {}


def _linkify_markdown(text: str) -> str:
    lines = str(text).splitlines()
    linked: list[str] = []
    in_fence = False
    for line in lines:
        stripped = line.lstrip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            in_fence = not in_fence
            linked.append(line)
            continue
        if in_fence:
            linked.append(line)
            continue
        if "](" in line:
            linked.append(line)
            continue
        linked.append(_linkify_line(line))
    return "\n".join(linked)


_INLINE_ABS_PATH_CODE_RE = re.compile(r"`(/[^`]+)`")
_HEADING_LINE_RE = re.compile(r"^\s{0,3}#{1,6}\s+(.*)$")
_UNORDERED_LIST_LINE_RE = re.compile(r"^(\s*)([-*])\s+(.+)$")
_ORDERED_LIST_LINE_RE = re.compile(r"^(\s*)(\d+)[.)]\s+(.+)$")
_COMPACT_INLINE_MARKDOWN_RE = re.compile(
    r"\*\*(?P<bold>[^*\n]+)\*\*|`(?P<code>[^`\n]+)`|\[(?P<label>[^\]\n]+)\]\((?P<target>[^)\n]+)\)"
)
_BARE_URL_RE = re.compile(r"(?<![<(])(https?://[^\s<>()]+)")
_ABS_PATH_RE = re.compile(r"(?<![\w`\\])(/(?!/)(?:[^\s`\]\),;]+/?)+)")
_LINK_CHUNK_SIZE = 74


def _linkify_line(line: str) -> str:
    def url_repl(match: re.Match[str]) -> str:
        raw = match.group(1)
        suffix = ""
        while raw and raw[-1] in ".,;:":
            suffix = raw[-1] + suffix
            raw = raw[:-1]
        return f"<{raw}>{suffix}"

    return _BARE_URL_RE.sub(url_repl, line)


def _chunked_markdown_link(label: str, target: str) -> str:
    chunks = [label[index : index + _LINK_CHUNK_SIZE] for index in range(0, len(label), _LINK_CHUNK_SIZE)]
    clean_target = _markdown_link_target(target)
    return "\n".join(f"[{_markdown_link_label(chunk)}](<{clean_target}>)" for chunk in chunks)


def _markdown_link_label(label: str) -> str:
    return label.replace("\\", "\\\\").replace("[", "\\[").replace("]", "\\]")


def _markdown_link_target(target: str) -> str:
    return target.replace("\n", "").replace(">", "%3E")


def _file_links_for_text(text: str, limit: int = 3) -> list[Path]:
    paths: list[Path] = []
    seen: set[Path] = set()
    for _start, _end, raw, path in _file_link_spans_for_text(text):
        resolved = path.resolve()
        if resolved not in seen:
            paths.append(resolved)
            seen.add(resolved)
        if len(paths) >= limit:
            break
    return paths[:limit]


def _rich_text_with_file_links(text: str, style: str = "none") -> Text:
    base_style = None if style == "none" else style
    rendered = Text(style=base_style)
    cursor = 0
    for start, end, label, target in _link_spans_for_text(text):
        if start < cursor:
            continue
        rendered.append(text[cursor:start])
        rendered.append(label, style=f"#8aa6a0 underline link {target}")
        cursor = end
    rendered.append(text[cursor:])
    return rendered


def _prompt_width(log: RichLog, *, fallback: int = 100) -> int:
    widths = [max(0, int(fallback or 0) - 2)]
    min_width = int(getattr(log, "min_width", 0) or 0)
    if min_width > 0:
        widths.append(min_width - 2)
    for attr in ("content_region", "region", "size"):
        value = getattr(log, attr, None)
        width = int(getattr(value, "width", 0) or 0)
        if width > 0:
            widths.append(width - 2)
    return max(24, max(widths) if widths else int(fallback or 100) - 2)


def _prompt_text(text: str, width: int = 100) -> Padding:
    width = max(24, int(width or 100))
    prefix_width = 2
    horizontal_padding = 2
    content_width = max(12, width - horizontal_padding - prefix_width)
    rendered = Text()
    muted_style = "bold #5c9cf5 on #1e1e1e"
    text_style = "bold #e4e4e7 on #1e1e1e"
    fill_style = "on #1e1e1e"
    source_lines = str(text or "").splitlines() or [""]
    visual_lines: list[str] = []
    for source_line in source_lines:
        visual_lines.extend(textwrap.wrap(source_line, width=content_width) or [""])
    for index, line in enumerate(visual_lines):
        if index:
            rendered.append("\n")
        prefix = "› " if index == 0 else "  "
        rendered.append(prefix, style=muted_style)
        rendered.append(line, style=text_style)
        rendered.append("\u00a0" * max(0, content_width - len(line)), style=fill_style)
    return Padding(rendered, (0, 1), style="on #1e1e1e", expand=True)


def _link_spans_for_text(text: str) -> list[tuple[int, int, str, str]]:
    spans: list[tuple[int, int, str, str]] = []
    covered: list[tuple[int, int]] = []
    for start, end, label, path in _file_link_spans_for_text(text):
        spans.append((start, end, label, path.resolve().as_uri()))
        covered.append((start, end))
    for match in _BARE_URL_RE.finditer(text):
        if any(start <= match.start() < end for start, end in covered):
            continue
        raw = match.group(1)
        end = match.start(1) + len(raw)
        while raw and raw[-1] in ".,;:":
            raw = raw[:-1]
            end -= 1
        if raw:
            spans.append((match.start(1), end, raw, raw))
    return sorted(spans, key=lambda item: item[0])


def _strip_inline_code_backticks(text: str) -> str:
    return re.sub(r"`([^`\n]+)`", r"\1", text)


def _file_link_spans_for_text(text: str) -> list[tuple[int, int, str, Path]]:
    spans: list[tuple[int, int, str, Path]] = []
    covered: list[tuple[int, int]] = []
    for match in _INLINE_ABS_PATH_CODE_RE.finditer(text):
        raw = match.group(1)
        path = Path(raw).expanduser()
        if path.exists():
            spans.append((match.start(), match.end(), raw, path))
            covered.append((match.start(), match.end()))
    for match in _ABS_PATH_RE.finditer(text):
        if any(start <= match.start() < end for start, end in covered):
            continue
        raw = match.group(1)
        while raw and raw[-1] in ".,;:":
            raw = raw[:-1]
        path = Path(raw).expanduser()
        if path.exists():
            spans.append((match.start(), match.start() + len(raw), raw, path))
    return sorted(spans, key=lambda item: item[0])


def _canonical_transcript_text(text: str) -> str:
    stripped = str(text or "").strip()
    if stripped.startswith("done:\n\n"):
        stripped = stripped.removeprefix("done:\n\n").strip()
    return stripped


def _join_transcript_text(previous: str, current: str) -> str:
    previous = _canonical_transcript_text(previous)
    current = _canonical_transcript_text(current)
    if not previous:
        return current
    if not current:
        return previous
    joined = f"{previous}\n\n{current}"
    return joined[-12000:]


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


def _composer_visible_line_count(text: str, width: int, *, max_lines: int = 5) -> int:
    available = max(20, int(width or 80) - 2)
    total = 0
    for line in str(text or "").split("\n"):
        total += max(1, (len(line) + available - 1) // available)
    return max(1, min(max_lines, total))


def _compact_result_text(value: object, limit: int = 160) -> str:
    text = str(value or "")
    for path in _file_links_for_text(text, limit=8):
        text = text.replace(str(path), path.name)
    return _compact_inline(text, limit=limit)


def _compact_error_text(value: object, limit: int = 220) -> str:
    text = " ".join(str(value or "").strip().split())
    message_match = re.search(r'"message"\s*:\s*"([^"]+)"', text)
    if message_match:
        prefix = text.split("{", 1)[0].strip().rstrip(":")
        message = message_match.group(1)
        text = f"{prefix}: {message}" if prefix else message
    return _compact_inline(text, limit=limit)


def _sidebar_result_lines(value: object) -> list[str]:
    text = str(value or "").strip()
    files = _file_links_for_text(text, limit=1)
    if files:
        path = files[0]
        return [f"[#9b9ca5]file[/] {_rich_link(path.name, path.resolve().as_uri())}"]
    table_rows = _markdown_table_row_count(text)
    if table_rows:
        label = "row" if table_rows == 1 else "rows"
        return [f"[#9b9ca5]table: {table_rows} {label}[/]"]
    if text.startswith("failed:"):
        text = _compact_error_text(text.removeprefix("failed:").strip(), limit=96)
    return [f"[#9b9ca5]{escape(_compact_result_text(text, limit=64))}[/]"]


def _footer_hint(key: str, label: str, *, active: bool = False) -> str:
    key_bg = "#2c3040" if active else "#23252d"
    key_fg = "#e4e4e7" if active else "#b8bac3"
    label_fg = "#e4e4e7" if active else "#8f929c"
    return (
        f"[bold {key_fg} on {key_bg}] {escape(key)} [/]"
        f" [#{label_fg.lstrip('#')}]{escape(label)}[/]"
    )


def _footer_hintbar_markup(items: list[tuple[str, str, bool]], *, width: int) -> str:
    visible_items = list(items)
    available = max(24, int(width or 80) - 2)
    while len(visible_items) > 1 and _footer_hintbar_visible_width(visible_items) > available:
        visible_items.pop()
    return "  [#3f424c]│[/]  ".join(
        _footer_hint(key, label, active=active) for key, label, active in visible_items
    )


def _footer_hintbar_visible_width(items: list[tuple[str, str, bool]]) -> int:
    if not items:
        return 0
    item_width = sum(len(key) + len(label) + 3 for key, label, _active in items)
    separator_width = 5 * (len(items) - 1)
    return item_width + separator_width


def _sidebar_image_line(value: str) -> str:
    text = value
    if text.startswith("screenshot -> "):
        text = text.removeprefix("screenshot -> ")
    return f"[#9b9ca5]screenshot: {escape(_compact_inline(text, limit=42))}[/]"


def _markdown_table_row_count(text: str) -> int:
    rows = 0
    for line in text.splitlines():
        stripped = line.strip()
        if not (stripped.startswith("|") and stripped.endswith("|")):
            continue
        if re.fullmatch(r"[\s|:\-]+", stripped):
            continue
        rows += 1
    return max(0, rows - 1)


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
    compact_run = _dataset_run_label_from_id(run_id)
    label = compact_run[:18]
    return f"{label}:{task_id}" if task_id else label


def _dataset_run_label_from_id(run_id: str) -> str:
    compact_run = run_id
    if compact_run.startswith("real-v8-"):
        compact_run = "v8-" + compact_run.removeprefix("real-v8-")
    if compact_run.startswith("real-v14-"):
        compact_run = "v14-" + compact_run.removeprefix("real-v14-")
    return compact_run


def _progress_bar(done: int, total: int, width: int = 12) -> str:
    width = max(4, width)
    if total <= 0:
        filled = 0
    else:
        filled = min(width, max(0, round((done / total) * width)))
    empty = width - filled
    return "[#e4e4e7]" + ("█" * filled) + "[/][#3a3d47]" + ("░" * empty) + "[/]"


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


def _latest_browser_live_url(events: list[Event]) -> str:
    for event in reversed(events):
        if event.type != "browser.live_url":
            continue
        payload = event.payload
        url = str(payload.get("live_url") or payload.get("url") or "").strip()
        if url:
            return url
    return ""


def _compact_live_preview_url(url: str) -> str:
    label = url.removeprefix("https://")
    return _compact_inline(label, limit=78)


def _rich_link(label: str, url: str) -> str:
    target = url.replace('"', "%22").replace("]", "%5D").replace("\n", "")
    return f"[link=\"{target}\"][#8aa6a0]{escape(label)}[/][/link]"


def _short_task_list(task_ids: list[str], limit: int = 12) -> str:
    if not task_ids:
        return "-"
    rendered = ", ".join(str(task_id) for task_id in task_ids[:limit])
    if len(task_ids) > limit:
        rendered += f" +{len(task_ids) - limit}"
    return rendered


def _status_markup(status: str) -> str:
    styles = {
        "running": "bold #e4e4e7",
        "done": "bold #e4e4e7",
        "failed": "bold #e4e4e7",
        "cancelled": "bold #9b9ca5",
        "created": "#9b9ca5",
    }
    return f"[{styles.get(status, '#e4e4e7')}]{escape(status)}[/]"


def _status_text(status: str) -> Text:
    styles = {
        "running": "bold #e4e4e7",
        "done": "bold #e4e4e7",
        "failed": "bold #e4e4e7",
        "cancelled": "bold #9b9ca5",
        "created": "#9b9ca5",
    }
    label = status[:9]
    return Text(label, style=styles.get(status, "#e4e4e7"))


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
            return f"failed: {_compact_error_text(event.payload.get('error') or 'failed')}"
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
        return suffix.lstrip(".") or "workspace"
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


def _assign_config_value(config: dict[str, Any], dotted: str, value: Any) -> None:
    parts = [part for part in dotted.split(".") if part]
    if not parts:
        return
    target: dict[str, Any] = config
    for part in parts[:-1]:
        existing = target.get(part)
        if not isinstance(existing, dict):
            existing = {}
            target[part] = existing
        target = existing
    target[parts[-1]] = value


class TextualTui:
    def __init__(
        self,
        store: SessionStore,
        provider_factory: Optional[ProviderFactory] = None,
        max_turns: int = 80,
        provider_label: str = "fake",
        model_label: Optional[str] = None,
        config: Optional[dict] = None,
        config_path: Optional[Path | str] = None,
    ) -> None:
        self.app = BrowserUseTerminalApp(
            store,
            provider_factory=provider_factory,
            max_turns=max_turns,
            provider_label=provider_label,
            model_label=model_label,
            config=config,
            config_path=config_path,
        )

    def run(self) -> int:
        self.app.run(mouse=_textual_mouse_enabled())
        return 0


def _textual_mouse_enabled() -> bool:
    configured = os.environ.get("LLM_BROWSER_TUI_MOUSE")
    if configured is not None:
        return configured.strip().lower() in {"1", "true", "yes", "on"}
    return bool(os.environ.get("TMUX"))
