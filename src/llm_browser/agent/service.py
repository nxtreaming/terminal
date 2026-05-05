from __future__ import annotations

from pathlib import Path
from typing import Any, Dict, List, Optional

from llm_browser.provider.base import Provider
from llm_browser.provider.fake import FakeProvider
from llm_browser.provider.types import ModelEvent, ToolCall
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tool.builtins import build_builtin_registry
from llm_browser.tool.context import ToolContext
from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolResult


class Agent:
    def __init__(
        self,
        store: SessionStore,
        provider: Optional[Provider] = None,
        tools: Optional[ToolRegistry] = None,
        max_turns: int = 20,
    ) -> None:
        self.store = store
        self.provider = provider or FakeProvider()
        self.tools = tools or build_builtin_registry()
        self.max_turns = max_turns

    def run(
        self,
        task: str,
        parent_id: Optional[str] = None,
        cwd: Optional[Path] = None,
    ) -> SessionMetadata:
        session = self.store.create(parent_id=parent_id, cwd=cwd)
        self.store.emit(session.id, "session.input", {"text": task})
        self.store.update_status(session.id, "running")

        messages: List[Dict[str, Any]] = [{"role": "user", "content": task}]
        final_result: Optional[str] = None

        try:
            for _ in range(self.max_turns):
                tool_calls: List[ToolCall] = []
                for event in self.provider.start_turn(messages, self.tools.specs()):
                    if event.type == "text_delta":
                        self.store.emit(session.id, "model.delta", {"text": event.text})
                    elif event.type == "tool_call":
                        if event.tool_call is None:
                            raise RuntimeError("provider emitted tool_call without a call")
                        tool_calls.append(event.tool_call)
                    elif event.type == "done":
                        pass
                    else:
                        raise RuntimeError(f"unknown provider event type: {event.type}")

                if not tool_calls:
                    break

                for call in tool_calls:
                    result = self._execute_tool(session.id, call)
                    messages.append(
                        {
                            "role": "tool",
                            "tool_call_id": call.id,
                            "name": call.name,
                            "content": result.to_provider_content(),
                        }
                    )
                    if call.name == "done":
                        final_result = result.text
                        break

                if final_result is not None:
                    break

            session = self.store.update_status(session.id, "done")
            self.store.emit(session.id, "session.done", {"result": final_result})
            self.tools.close_session(session.id)
            return session
        except BaseException as exc:
            self.store.update_status(session.id, "failed")
            self.store.emit(
                session.id,
                "session.failed",
                {"error": str(exc), "error_type": type(exc).__name__},
            )
            self.tools.close_session(session.id)
            raise

    def _execute_tool(self, session_id: str, call: ToolCall) -> ToolResult:
        session = self.store.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")
        ctx = ToolContext(session=session, store=self.store, tool_call_id=call.id, tool_name=call.name)
        self.store.emit(
            session_id,
            "tool.started",
            {"tool_call_id": call.id, "name": call.name, "arguments": call.arguments},
        )
        try:
            result = self.tools.run(call.name, call.arguments, ctx)
            self.store.emit(
                session_id,
                "tool.finished",
                {
                    "tool_call_id": call.id,
                    "name": call.name,
                    "output": result.to_event_payload(),
                },
            )
            return result
        except BaseException as exc:
            self.store.emit(
                session_id,
                "tool.failed",
                {
                    "tool_call_id": call.id,
                    "name": call.name,
                    "error": str(exc),
                    "error_type": type(exc).__name__,
                },
            )
            raise
