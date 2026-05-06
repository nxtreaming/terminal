from __future__ import annotations

import json
import threading
import time
import traceback
from pathlib import Path
from typing import Any, Callable, Optional

from llm_browser.provider.base import Provider
from llm_browser.session.store import SessionStore
from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult
from llm_browser.tool.spec import ToolSpec


ProviderFactory = Callable[[], Optional[Provider]]
FINAL_SESSION_STATUSES = {"done", "failed", "cancelled"}
SUBAGENT_ROLE_GUIDANCE = {
    "default": "Handle the assigned task directly. Report concise findings and changed files when applicable.",
    "explorer": (
        "Answer specific, well-scoped codebase questions. Prefer rg/rg --files, read only the files needed, "
        "and return concrete file references and behavioral conclusions."
    ),
    "worker": (
        "Implement the assigned bounded change in the requested files/modules. You are not alone in the codebase: "
        "do not revert edits made by others, and adapt your work to surrounding changes. List changed paths in the final answer."
    ),
}


class SessionTool:
    """Model-visible normal-session primitive.

    Child/background agents are ordinary sessions with parent_id. The tool
    returns session ids and event cursors; callers inspect progress by reading
    events instead of blocking on a special wait abstraction.
    """

    def __init__(
        self,
        store: SessionStore,
        provider_factory: ProviderFactory,
        max_turns: int = 80,
        mode: str = "auto",
    ) -> None:
        self.store = store
        self.provider_factory = provider_factory
        self.max_turns = max_turns
        self.mode = mode
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

    def spawn_agent(self, ctx: ToolContext, arguments: dict) -> ToolResult:
        agent_type = str(arguments.get("agent_type") or "default")
        if agent_type not in SUBAGENT_ROLE_GUIDANCE:
            raise ValueError(f"unknown agent_type: {agent_type}")
        message = _message_from_subagent_arguments(arguments)
        if not message.strip():
            raise ValueError("spawn_agent requires message or items")
        prompt = _subagent_prompt(agent_type, message)
        result = self._create(
            ctx,
            {
                "prompt": prompt,
                "parent_id": ctx.session.id,
                "cwd": arguments.get("cwd"),
            },
        )
        session = result.data.get("session", {}) if isinstance(result.data, dict) else {}
        agent_id = str(session.get("id") or "")
        payload = {
            "agent_id": agent_id,
            "nickname": agent_type,
            "session": session,
            "running": True,
        }
        return ToolResult(text=f"spawned {agent_type} agent {agent_id}", data=payload)

    def wait_agent(self, ctx: ToolContext, arguments: dict) -> ToolResult:
        raw_targets = arguments.get("targets") or []
        if isinstance(raw_targets, str):
            targets = [raw_targets]
        else:
            targets = [str(target) for target in raw_targets]
        targets = [target for target in targets if target]
        if not targets:
            raise ValueError("wait_agent requires targets")
        timeout_ms = _bounded_timeout_ms(arguments.get("timeout_ms"), default=30000)
        deadline = time.monotonic() + timeout_ms / 1000
        statuses: dict[str, Any] = {}

        while True:
            statuses = {target: self._agent_status_payload(target) for target in targets}
            if any(status.get("status") in FINAL_SESSION_STATUSES or status.get("status") == "not_found" for status in statuses.values()):
                break
            if time.monotonic() >= deadline:
                break
            if ctx.is_cancel_requested():
                break
            time.sleep(0.05)

        completed = {
            target: status
            for target, status in statuses.items()
            if status.get("status") in FINAL_SESSION_STATUSES or status.get("status") == "not_found"
        }
        payload = {
            "statuses": statuses,
            "completed": completed,
            "timed_out": not completed,
        }
        return ToolResult(text=json.dumps(payload, indent=2), data=payload)

    def close_agent(self, ctx: ToolContext, arguments: dict) -> ToolResult:
        target = str(arguments.get("target") or "")
        if not target:
            raise ValueError("close_agent requires target")
        previous = self._agent_status_payload(target)
        for session_id in self._descendant_session_ids(target):
            try:
                self.store.request_cancel(session_id, reason="close_agent requested cancellation")
            except KeyError:
                pass
        try:
            self.store.request_cancel(target, reason="close_agent requested cancellation")
        except KeyError:
            pass
        payload = {"target": target, "previous_status": previous}
        return ToolResult(text=json.dumps(payload, indent=2), data=payload)

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

    def _agent_status_payload(self, session_id: str) -> dict[str, Any]:
        session = self.store.load(session_id)
        if session is None:
            return {"agent_id": session_id, "status": "not_found"}
        events = self.store.events.read(session_id)
        latest = events[-1].to_dict() if events else None
        final_event = next(
            (
                event
                for event in reversed(events)
                if event.type in {"session.done", "session.failed", "session.cancelled"}
            ),
            None,
        )
        payload: dict[str, Any] = {
            "agent_id": session_id,
            "status": session.status,
            "session": session.to_dict(),
            "events": len(events),
            "latest_event": latest,
        }
        if final_event is not None:
            payload["final_event"] = final_event.to_dict()
        return payload

    def _descendant_session_ids(self, parent_id: str) -> list[str]:
        descendants: list[str] = []
        pending = [parent_id]
        while pending:
            current = pending.pop()
            children = [session.id for session in self.store.list() if session.parent_id == current]
            descendants.extend(children)
            pending.extend(children)
        return descendants

    def _start_runner(self, session_id: str, prompt: str, resume: bool) -> None:
        def target() -> None:
            from llm_browser.agent.service import Agent

            try:
                agent = Agent(
                    self.store,
                    provider_factory=self.provider_factory,
                    max_turns=self.max_turns,
                    mode=self.mode,
                )
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


def spawn_agent_tool_spec() -> ToolSpec:
    return ToolSpec(
        name="spawn_agent",
        description=(
            "Start a Codex-shaped background subagent as a normal child session. "
            "Use only when the user explicitly asks for subagents, delegation, or parallel agent work."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "agent_type": {"type": "string", "enum": ["default", "explorer", "worker"]},
                "message": {"type": "string"},
                "items": {"type": "array", "items": {"type": "object"}},
                "fork_context": {"type": "boolean"},
                "cwd": {"type": "string"},
                "model": {"type": "string"},
                "reasoning_effort": {"type": "string"},
            },
            "additionalProperties": False,
        },
    )


def wait_agent_tool_spec() -> ToolSpec:
    return ToolSpec(
        name="wait_agent",
        description="Wait for one or more subagents to reach a final status and return their session status/events summary.",
        input_schema={
            "type": "object",
            "properties": {
                "targets": {"type": "array", "items": {"type": "string"}},
                "timeout_ms": {"type": "integer"},
            },
            "required": ["targets"],
            "additionalProperties": False,
        },
    )


def close_agent_tool_spec() -> ToolSpec:
    return ToolSpec(
        name="close_agent",
        description="Request cancellation for a subagent session and its child sessions, returning the previous status.",
        input_schema={
            "type": "object",
            "properties": {"target": {"type": "string"}},
            "required": ["target"],
            "additionalProperties": False,
        },
    )


def _message_from_subagent_arguments(arguments: dict) -> str:
    if "message" in arguments and arguments.get("message") is not None:
        return str(arguments.get("message") or "")
    items = arguments.get("items") or []
    if not isinstance(items, list):
        raise ValueError("spawn_agent items must be a list")
    parts: list[str] = []
    for item in items:
        if not isinstance(item, dict):
            continue
        if item.get("type") == "text":
            parts.append(str(item.get("text") or ""))
        elif item.get("path"):
            parts.append(f"{item.get('type', 'item')}: {item.get('path')}")
        elif item.get("name"):
            parts.append(f"{item.get('type', 'item')}: {item.get('name')}")
    return "\n".join(part for part in parts if part.strip())


def _subagent_prompt(agent_type: str, message: str) -> str:
    guidance = SUBAGENT_ROLE_GUIDANCE[agent_type]
    return (
        f"You are a Codex subagent with role: {agent_type}.\n"
        f"{guidance}\n\n"
        "Assigned task:\n"
        f"{message}"
    )


def _bounded_timeout_ms(value: Any, default: int) -> int:
    try:
        number = int(value if value is not None else default)
    except (TypeError, ValueError):
        number = default
    return min(max(number, 0), 3_600_000)
