from __future__ import annotations

import queue
import shlex
import threading
from typing import Callable, Optional

from llm_browser.agent import Agent
from llm_browser.brand import PRODUCT_NAME
from llm_browser.events import Event
from llm_browser.provider.base import Provider
from llm_browser.session.store import SessionStore


ProviderFactory = Callable[[], Optional[Provider]]


class SimpleTui:
    def __init__(self, store: SessionStore, provider_factory: Optional[ProviderFactory] = None) -> None:
        self.store = store
        self.provider_factory = provider_factory or (lambda: None)

    def run(self) -> int:
        print(PRODUCT_NAME)
        print("commands: run <task>, sessions, show <session_id>, quit")
        while True:
            try:
                line = input("but> ").strip()
            except (EOFError, KeyboardInterrupt):
                print()
                return 0
            if not line:
                continue
            try:
                args = shlex.split(line)
            except ValueError as exc:
                print(f"parse error: {exc}")
                continue
            command = args[0]
            if command in {"quit", "exit"}:
                return 0
            if command == "sessions":
                self._print_sessions()
                continue
            if command == "show" and len(args) == 2:
                self._show(args[1])
                continue
            if command == "run" and len(args) >= 2:
                self._run_task(" ".join(args[1:]))
                continue
            print("unknown command")

    def _run_task(self, task: str) -> None:
        agent = Agent(self.store, provider_factory=self.provider_factory)
        error = []

        def target() -> None:
            try:
                agent.run(task)
            except BaseException as exc:
                error.append(exc)

        with self.store.bus.subscribe() as events:
            thread = threading.Thread(target=target, daemon=True)
            thread.start()
            while thread.is_alive() or not events.empty():
                try:
                    event = events.get(timeout=0.1)
                except queue.Empty:
                    continue
                print(format_event(event))
            thread.join()
        if error:
            print(f"run failed: {error[0]}")

    def _print_sessions(self) -> None:
        for session in self.store.list():
            print(f"{session.id}  {session.status:10}  {session.cwd}")

    def _show(self, session_id: str) -> None:
        session = self.store.load(session_id)
        if session is None:
            print(f"session not found: {session_id}")
            return
        print(f"{session.id}  {session.status}  {session.cwd}")
        for event in self.store.events.read(session.id):
            print(format_event(event))


def format_event(event: Event) -> str:
    payload = event.payload
    if event.type == "session.created":
        return f"[{event.session_id}] session created"
    if event.type == "session.input":
        return f"[{event.session_id}] user: {_compact_text(payload.get('text', ''), limit=240)}"
    if event.type == "session.status":
        return f"[{event.session_id}] status: {payload.get('status', '')}"
    if event.type == "session.cancel_requested":
        return f"[{event.session_id}] cancel requested: {payload.get('reason', '')}"
    if event.type == "session.compacted":
        return f"[{event.session_id}] compacted: {payload.get('before_messages')} -> {payload.get('after_messages')} messages"
    if event.type == "session.deadline_warning":
        return f"[{event.session_id}] deadline warning: {payload.get('remaining_s')}s remaining"
    if event.type == "model.delta":
        return f"[{event.session_id}] model: {payload.get('text', '').rstrip()}"
    if event.type == "tool.started":
        return f"[{event.session_id}] tool start: {payload.get('name')} {payload.get('tool_call_id')}"
    if event.type == "tool.image":
        image = payload.get("image") or {}
        return f"[{event.session_id}] image: {image.get('label')} -> {image.get('path')}"
    if event.type == "tool.output":
        text = _compact_text(payload.get("text", ""))
        return f"[{event.session_id}] tool output: {payload.get('name')} {payload.get('stream')} {text}"
    if event.type == "tool.finished":
        output = payload.get("output") or {}
        text = _compact_text(output.get("text", ""))
        return f"[{event.session_id}] tool done: {payload.get('name')} {text}"
    if event.type == "tool.failed":
        return f"[{event.session_id}] tool failed: {payload.get('name')} {payload.get('error')}"
    if event.type == "session.done":
        return f"[{event.session_id}] done: {_compact_text(payload.get('result', ''), limit=240)}"
    if event.type == "session.failed":
        return f"[{event.session_id}] failed: {payload.get('error')}"
    return f"[{event.session_id}] {event.type}: {payload}"


def _compact_text(value: object, limit: int = 160) -> str:
    text = str(value or "").strip().replace("\n", "\\n")
    if len(text) > limit:
        return text[: max(0, limit - 3)] + "..."
    return text
