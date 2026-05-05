from __future__ import annotations

from pathlib import Path
from typing import Any, Dict, List, Optional

from llm_browser.provider.base import Provider
from llm_browser.provider.fake import FakeProvider
from llm_browser.provider.types import ModelEvent, ToolCall
from llm_browser.session.cancel import SessionCancelled
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tool.builtins import build_builtin_registry
from llm_browser.tool.context import ToolContext
from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolResult

MAX_INLINE_TOOL_TEXT = 20000


class MaxTurnsExceeded(RuntimeError):
    pass


class Agent:
    def __init__(
        self,
        store: SessionStore,
        provider: Optional[Provider] = None,
        tools: Optional[ToolRegistry] = None,
        max_turns: int = 80,
        recover_tool_errors: bool = True,
    ) -> None:
        self.store = store
        self.provider = provider or FakeProvider()
        self.tools = tools or build_builtin_registry()
        self.max_turns = max_turns
        self.recover_tool_errors = recover_tool_errors

    def run(
        self,
        task: str,
        parent_id: Optional[str] = None,
        cwd: Optional[Path] = None,
    ) -> SessionMetadata:
        session = self.store.create(parent_id=parent_id, cwd=cwd)
        return self.run_session(session.id, task)

    def run_session(self, session_id: str, task: str) -> SessionMetadata:
        session = self.store.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")

        self.store.clear_cancel(session.id)
        self.store.emit(session.id, "session.input", {"text": task})
        self.store.update_status(session.id, "running")

        messages: List[Dict[str, Any]] = [{"role": "user", "content": task}]
        final_result: Optional[str] = None

        try:
            for _ in range(self.max_turns):
                self._check_cancel(session.id)
                tool_calls: List[ToolCall] = []
                for event in self.provider.start_turn(messages, self.tools.specs()):
                    self._check_cancel(session.id)
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

                messages.append(
                    {
                        "role": "assistant",
                        "tool_calls": [
                            {"id": call.id, "name": call.name, "arguments": call.arguments}
                            for call in tool_calls
                        ],
                    }
                )
                for call in tool_calls:
                    self._check_cancel(session.id)
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

            if final_result is None:
                raise MaxTurnsExceeded(f"model did not call done within {self.max_turns} turns")

            session = self.store.update_status(session.id, "done")
            self.store.emit(session.id, "session.done", {"result": final_result})
            return session
        except SessionCancelled as exc:
            session = self.store.update_status(session.id, "cancelled")
            self.store.emit(session.id, "session.cancelled", {"reason": exc.reason})
            return session
        except Exception as exc:
            self.store.update_status(session.id, "failed")
            self.store.emit(
                session.id,
                "session.failed",
                {"error": str(exc), "error_type": type(exc).__name__},
            )
            raise
        finally:
            self.tools.close_session(session.id)

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
            result = self._spill_large_tool_output(ctx, call, result)
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
        except SessionCancelled:
            raise
        except Exception as exc:
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
            if not self.recover_tool_errors:
                raise
            return ToolResult(
                text=f"[tool error: {type(exc).__name__}: {exc}]",
                data={"ok": False, "error": str(exc), "error_type": type(exc).__name__},
            )

    def _spill_large_tool_output(self, ctx: ToolContext, call: ToolCall, result: ToolResult) -> ToolResult:
        if len(result.text) <= MAX_INLINE_TOOL_TEXT:
            return result

        output_dir = ctx.session.artifact_dir / "tool-output"
        output_dir.mkdir(parents=True, exist_ok=True)
        path = output_dir / f"{call.id}_{call.name}.txt"
        path.write_text(result.text, encoding="utf-8")
        data = dict(result.data)
        data["truncated"] = True
        data["output_path"] = str(path)
        text = result.text[:MAX_INLINE_TOOL_TEXT] + f"\n\n[full output saved to {path}]"
        return ToolResult(text=text, data=data, images=result.images)

    def _check_cancel(self, session_id: str) -> None:
        request = self.store.cancel_request(session_id)
        if request is not None:
            raise SessionCancelled(session_id, request["reason"])
