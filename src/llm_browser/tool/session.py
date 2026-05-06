from __future__ import annotations

import json
import threading
import traceback
from pathlib import Path
from typing import Callable, Optional

from llm_browser.provider.base import Provider
from llm_browser.session.store import SessionStore
from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult
from llm_browser.tool.spec import ToolSpec


ProviderFactory = Callable[[], Optional[Provider]]


class SessionTool:
    """Model-visible normal-session primitive.

    Child/background agents are ordinary sessions with parent_id. The tool
    returns session ids and event cursors; callers inspect progress by reading
    events instead of blocking on a special wait abstraction.
    """

    def __init__(self, store: SessionStore, provider_factory: ProviderFactory, max_turns: int = 80) -> None:
        self.store = store
        self.provider_factory = provider_factory
        self.max_turns = max_turns
        self._lock = threading.Lock()
        self._threads: dict[str, threading.Thread] = {}

    def __call__(self, ctx: ToolContext, arguments: dict) -> ToolResult:
        action = str(arguments.get("action") or "")
        if action == "create":
            return self._create(ctx, arguments)
        if action == "resume":
            return self._resume(ctx, arguments)
        if action == "cancel":
            return self._cancel(arguments)
        if action == "status":
            return self._status(arguments)
        if action == "list":
            return self._list(arguments)
        if action == "read":
            return self._read(arguments)
        raise ValueError(f"unknown session action: {action}")

    def _create(self, ctx: ToolContext, arguments: dict) -> ToolResult:
        prompt = str(arguments.get("prompt") or "")
        if not prompt.strip():
            raise ValueError("session create requires a non-empty prompt")
        parent_id = str(arguments.get("parent_id") or ctx.session.id)
        cwd_value = arguments.get("cwd")
        cwd = Path(str(cwd_value)).expanduser().resolve() if cwd_value else ctx.session.cwd
        child = self.store.create(parent_id=parent_id, cwd=cwd)
        self.store.emit(ctx.session.id, "session.child_started", {"child_id": child.id, "prompt": prompt[:500]})
        self.store.emit(child.id, "session.parent", {"parent_id": parent_id})
        self._start_runner(child.id, prompt, resume=False)
        return ToolResult(
            text=f"started child session {child.id}",
            data={"session": child.to_dict(), "cursor": 0, "running": True},
        )

    def _resume(self, ctx: ToolContext, arguments: dict) -> ToolResult:
        session_id = str(arguments.get("session_id") or "")
        prompt = str(arguments.get("prompt") or "Continue from the previous session state.")
        session = self.store.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")
        if session.status == "running":
            raise RuntimeError(f"session is already running: {session_id}")
        self.store.emit(ctx.session.id, "session.child_resumed", {"child_id": session_id, "prompt": prompt[:500]})
        self._start_runner(session_id, prompt, resume=True)
        return ToolResult(text=f"resumed session {session_id}", data={"session_id": session_id, "running": True})

    def _cancel(self, arguments: dict) -> ToolResult:
        session_id = str(arguments.get("session_id") or "")
        reason = str(arguments.get("reason") or "session tool requested cancellation")
        self.store.request_cancel(session_id, reason=reason)
        return ToolResult(text=f"cancel requested for {session_id}", data={"session_id": session_id, "reason": reason})

    def _status(self, arguments: dict) -> ToolResult:
        session_id = str(arguments.get("session_id") or "")
        session = self.store.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")
        events = self.store.events.read(session_id)
        payload = {
            "session": session.to_dict(),
            "events": len(events),
            "latest_event": events[-1].to_dict() if events else None,
        }
        return ToolResult(text=json.dumps(payload, indent=2), data=payload)

    def _list(self, arguments: dict) -> ToolResult:
        parent_id = arguments.get("parent_id")
        limit = int(arguments.get("limit", 20))
        sessions = self.store.list()
        if parent_id is not None:
            sessions = [session for session in sessions if session.parent_id == str(parent_id)]
        rows = [session.to_dict() for session in sessions[:limit]]
        return ToolResult(text=json.dumps(rows, indent=2), data={"sessions": rows, "count": len(rows)})

    def _read(self, arguments: dict) -> ToolResult:
        session_id = str(arguments.get("session_id") or "")
        cursor = max(0, int(arguments.get("cursor", 0)))
        limit = int(arguments.get("limit", 50))
        events = self.store.events.read(session_id)
        selected = events[cursor : cursor + limit]
        payload = {
            "session_id": session_id,
            "cursor": cursor,
            "next_cursor": cursor + len(selected),
            "total_events": len(events),
            "events": [event.to_dict() for event in selected],
        }
        return ToolResult(text=json.dumps(payload, indent=2), data=payload)

    def _start_runner(self, session_id: str, prompt: str, resume: bool) -> None:
        def target() -> None:
            from llm_browser.agent.service import Agent

            try:
                agent = Agent(self.store, provider_factory=self.provider_factory, max_turns=self.max_turns)
                if resume:
                    agent.resume_session(session_id, prompt)
                else:
                    agent.run_session(session_id, prompt)
            except BaseException:
                self.store.update_status(session_id, "failed")
                self.store.emit(session_id, "session.failed", {"error": traceback.format_exc(), "error_type": "BackgroundSessionError"})

        thread = threading.Thread(target=target, name=f"browser-use-terminal-child-{session_id}", daemon=True)
        with self._lock:
            self._threads[session_id] = thread
        thread.start()

    def close_session(self, session_id: str) -> None:
        with self._lock:
            done = [key for key, thread in self._threads.items() if not thread.is_alive()]
            for key in done:
                del self._threads[key]


def session_tool_spec() -> ToolSpec:
    return ToolSpec(
        name="session",
        description=(
            "Create, resume, cancel, list, status, or read normal background sessions. "
            "Subagents are just sessions with parent_id; use read with a cursor to subscribe by polling events."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "resume", "cancel", "status", "list", "read"]},
                "prompt": {"type": "string"},
                "session_id": {"type": "string"},
                "parent_id": {"type": "string"},
                "cwd": {"type": "string"},
                "reason": {"type": "string"},
                "cursor": {"type": "integer"},
                "limit": {"type": "integer"},
            },
            "required": ["action"],
            "additionalProperties": False,
        },
    )
