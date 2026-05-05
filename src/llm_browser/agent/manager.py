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

    def start(self, task: str, parent_id: Optional[str] = None, cwd: Optional[Path] = None) -> SessionMetadata:
        session = self.store.create(parent_id=parent_id, cwd=cwd)

        active_ref: Dict[str, ActiveSession] = {}

        def target() -> None:
            try:
                provider = self.provider_factory()
                agent = Agent(self.store, provider=provider, max_turns=self.max_turns)
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

    def cancel(self, session_id: str, reason: str = "user requested cancellation") -> None:
        self.store.request_cancel(session_id, reason=reason)

    def active(self) -> Dict[str, ActiveSession]:
        with self._lock:
            return dict(self._active)

    def reap(self) -> None:
        with self._lock:
            done_ids = [session_id for session_id, active in self._active.items() if not active.running]
            for session_id in done_ids:
                del self._active[session_id]
