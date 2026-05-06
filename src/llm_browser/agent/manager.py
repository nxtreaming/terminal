from __future__ import annotations

import threading
import traceback
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Dict, Optional

from llm_browser.agent.service import Agent
from llm_browser.provider.base import Provider
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tool.registry import ToolRegistry


ProviderFactory = Callable[[], Optional[Provider]]


@dataclass
class ActiveSession:
    session_id: str
    task: str
    thread: threading.Thread
    error: Optional[str] = None

    @property
    def running(self) -> bool:
        return self.thread.is_alive()


class SessionManager:
    """In-process background runner for TUI and local orchestration."""

    def __init__(
        self,
        store: SessionStore,
        provider_factory: Optional[ProviderFactory] = None,
        max_turns: int = 80,
    ) -> None:
        self.store = store
        self.provider_factory = provider_factory or (lambda: None)
        self.max_turns = max_turns
        self._lock = threading.Lock()
        self._active: Dict[str, ActiveSession] = {}
        self._tools: Dict[str, ToolRegistry] = {}

    def start(self, task: str, parent_id: Optional[str] = None, cwd: Optional[Path] = None) -> SessionMetadata:
        session = self.store.create(parent_id=parent_id, cwd=cwd)

        active_ref: Dict[str, ActiveSession] = {}

        def target() -> None:
            try:
                agent = Agent(
                    self.store,
                    provider_factory=self.provider_factory,
                    max_turns=self.max_turns,
                    close_tools_on_finish=False,
                )
                with self._lock:
                    self._tools[session.id] = agent.tools
                agent.run_session(session.id, task)
            except BaseException:
                active_ref["active"].error = traceback.format_exc()

        thread = threading.Thread(target=target, name=f"browser-use-terminal-{session.id}", daemon=True)
        active = ActiveSession(session_id=session.id, task=task, thread=thread)
        active_ref["active"] = active
        with self._lock:
            self._active[session.id] = active
        thread.start()
        return session

    def resume(self, session_id: str, instruction: str = "Continue from the previous session state.") -> SessionMetadata:
        session = self.store.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")
        with self._lock:
            active = self._active.get(session.id)
            if active is not None and active.running:
                raise RuntimeError(f"session already running: {session.id}")

        active_ref: Dict[str, ActiveSession] = {}

        def target() -> None:
            try:
                with self._lock:
                    tools = self._tools.get(session.id)
                agent = Agent(
                    self.store,
                    provider_factory=self.provider_factory,
                    tools=tools,
                    max_turns=self.max_turns,
                    close_tools_on_finish=False,
                )
                with self._lock:
                    self._tools[session.id] = agent.tools
                agent.resume_session(session.id, instruction)
            except BaseException:
                active_ref["active"].error = traceback.format_exc()

        thread = threading.Thread(target=target, name=f"browser-use-terminal-{session.id}", daemon=True)
        resumed = ActiveSession(session_id=session.id, task=instruction, thread=thread)
        active_ref["active"] = resumed
        with self._lock:
            self._active[session.id] = resumed
        thread.start()
        return session

    def cancel(self, session_id: str, reason: str = "user requested cancellation") -> None:
        self.store.request_cancel(session_id, reason=reason)

    def close(self, session_id: str, stop_browser: bool = True) -> None:
        with self._lock:
            tools = self._tools.pop(session_id, None)
        if tools is not None:
            tools.close_session(session_id, stop_browser=stop_browser)

    def active(self) -> Dict[str, ActiveSession]:
        with self._lock:
            return dict(self._active)

    def reap(self) -> None:
        with self._lock:
            done_ids = [session_id for session_id, active in self._active.items() if not active.running]
            for session_id in done_ids:
                del self._active[session_id]
