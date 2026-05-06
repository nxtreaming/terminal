from __future__ import annotations

import concurrent.futures
import os
import time
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

from llm_browser.agent.compaction import compact_messages, message_chars
from llm_browser.provider.base import Provider
from llm_browser.provider.fake import FakeProvider
from llm_browser.provider.types import ModelEvent, ToolCall
from llm_browser.session.cancel import SessionCancelled
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tool.builtins import build_builtin_registry
from llm_browser.tool.context import ToolContext
from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolImage, ToolResult
from llm_browser.tool.session import SessionTool, session_tool_spec

MAX_INLINE_TOOL_TEXT = 20000
DEFAULT_COMPACT_AFTER_CHARS = 120000
PARALLEL_SAFE_TOOL_NAMES = {"echo", "read", "grep", "glob"}
PARALLEL_SAFE_SESSION_ACTIONS = {"read", "status", "list"}


class MaxTurnsExceeded(RuntimeError):
    pass


class Agent:
    def __init__(
        self,
        store: SessionStore,
        provider: Optional[Provider] = None,
        provider_factory: Optional[Callable[[], Optional[Provider]]] = None,
        tools: Optional[ToolRegistry] = None,
        max_turns: int = 80,
        recover_tool_errors: bool = True,
        compact_after_chars: int = DEFAULT_COMPACT_AFTER_CHARS,
        time_budget_s: Optional[float] = None,
    ) -> None:
        self.store = store
        self.provider_factory = provider_factory or (lambda: None)
        self.provider = provider or (provider_factory() if provider_factory else None) or FakeProvider()
        self.tools = tools or build_builtin_registry()
        self.max_turns = max_turns
        self.recover_tool_errors = recover_tool_errors
        self.compact_after_chars = compact_after_chars
        self.time_budget_s = time_budget_s
        self.deadline_at = time.monotonic() + time_budget_s if time_budget_s and time_budget_s > 0 else None
        self._deadline_warning_sent = False
        if tools is None:
            self.tools.register(
                session_tool_spec(),
                SessionTool(self.store, provider_factory=self._child_provider_factory, max_turns=self.max_turns),
            )

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
        return self._run_with_messages(session, messages)

    def resume_session(self, session_id: str, instruction: str = "Continue from the previous session state.") -> SessionMetadata:
        session = self.store.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")

        self.store.clear_cancel(session.id)
        messages = self._messages_from_events(session.id)
        messages.append({"role": "user", "content": instruction})
        self.store.emit(session.id, "session.input", {"text": instruction, "resumed": True})
        self.store.update_status(session.id, "running")
        return self._run_with_messages(session, messages)

    def _run_with_messages(self, session: SessionMetadata, messages: List[Dict[str, Any]]) -> SessionMetadata:
        final_result: Optional[str] = None
        runner_pid = os.getpid()
        self.store.begin_run(session.id, pid=runner_pid)

        try:
            for _ in range(self.max_turns):
                self._check_cancel(session.id)
                messages = self._maybe_add_deadline_warning(session, messages)
                messages = self._maybe_compact(session, messages)
                tool_calls: List[ToolCall] = []
                text_parts: List[str] = []
                for event in self.provider.start_turn(messages, self.tools.specs()):
                    self._check_cancel(session.id)
                    if event.type == "text_delta":
                        text_parts.append(event.text)
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
                    if text_parts:
                        final_result = "".join(text_parts).strip()
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
                for call, result in self._execute_tool_calls(session.id, tool_calls):
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
            self.store.clear_runner(session.id, pid=runner_pid)
            self.tools.close_session(session.id)

    def _messages_from_events(self, session_id: str) -> List[Dict[str, Any]]:
        messages: List[Dict[str, Any]] = []
        pending_tool_call: Optional[Dict[str, Any]] = None
        for event in self.store.events.read(session_id):
            if event.type == "session.input":
                text = str(event.payload.get("text") or "")
                if text:
                    messages.append({"role": "user", "content": text})
            elif event.type == "tool.started":
                pending_tool_call = {
                    "id": str(event.payload.get("tool_call_id") or ""),
                    "name": str(event.payload.get("name") or ""),
                    "arguments": event.payload.get("arguments") or {},
                }
                messages.append({"role": "assistant", "tool_calls": [pending_tool_call]})
            elif event.type == "tool.finished" and pending_tool_call:
                output = event.payload.get("output") or {}
                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": pending_tool_call["id"],
                        "name": pending_tool_call["name"],
                        "content": self._tool_event_provider_content(output),
                    }
                )
                pending_tool_call = None
            elif event.type == "tool.failed" and pending_tool_call:
                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": pending_tool_call["id"],
                        "name": pending_tool_call["name"],
                        "content": f"[tool error: {event.payload.get('error_type')}: {event.payload.get('error')}]",
                    }
                )
                pending_tool_call = None
        if not messages:
            raise ValueError(f"session has no replayable messages: {session_id}")
        return messages

    def _tool_event_provider_content(self, output: Any) -> Any:
        if not isinstance(output, dict):
            return str(output or "")
        images: List[ToolImage] = []
        for item in output.get("images") or []:
            if not isinstance(item, dict):
                continue
            try:
                images.append(ToolImage(**item))
            except TypeError:
                continue
        text = str(output.get("text") or "")
        data = output.get("data") if isinstance(output.get("data"), dict) else {}
        if images or data:
            return ToolResult(text=text, data=data, images=images).to_provider_content()
        return text

    def _maybe_compact(self, session: SessionMetadata, messages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        if self.compact_after_chars <= 0 or message_chars(messages) <= self.compact_after_chars:
            return messages
        compacted, path = compact_messages(
            messages,
            session.artifact_dir,
            session_events=[event.to_dict() for event in self.store.events.read(session.id)],
        )
        if compacted is not messages:
            self.store.emit(
                session.id,
                "session.compacted",
                {
                    "path": str(path),
                    "before_messages": len(messages),
                    "after_messages": len(compacted),
                    "before_chars": message_chars(messages),
                    "after_chars": message_chars(compacted),
                },
            )
        return compacted

    def _maybe_add_deadline_warning(self, session: SessionMetadata, messages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        if self.deadline_at is None or self._deadline_warning_sent:
            return messages
        remaining_s = self.deadline_at - time.monotonic()
        budget = self.time_budget_s or 0
        threshold_s = max(60.0, min(180.0, budget * 0.25))
        if remaining_s > threshold_s:
            return messages

        remaining_display = max(0, int(remaining_s))
        text = (
            f"Runtime note: about {remaining_display} seconds remain for this task. "
            "If you have enough evidence, stop exploratory work and call done with the best complete answer now. "
            "If the original site is unreliable, include the fallback evidence and any ambiguity instead of continuing indefinitely."
        )
        self._deadline_warning_sent = True
        self.store.emit(
            session.id,
            "session.deadline_warning",
            {"remaining_s": remaining_display, "text": text},
        )
        return [*messages, {"role": "user", "content": text}]

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
            event_result = self._spill_large_tool_output(ctx, call, result)
            self.store.emit(
                session_id,
                "tool.finished",
                {
                    "tool_call_id": call.id,
                    "name": call.name,
                    "output": event_result.to_event_payload(),
                },
            )
            return result if call.name == "done" else event_result
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

    def _execute_tool_calls(self, session_id: str, tool_calls: List[ToolCall]) -> List[tuple[ToolCall, ToolResult]]:
        executed: List[tuple[ToolCall, ToolResult]] = []
        index = 0
        while index < len(tool_calls):
            self._check_cancel(session_id)
            call = tool_calls[index]
            if call.name == "done" or not self._can_run_parallel(call):
                result = self._execute_tool(session_id, call)
                executed.append((call, result))
                index += 1
                if call.name == "done":
                    break
                continue

            batch: List[ToolCall] = []
            while index < len(tool_calls) and self._can_run_parallel(tool_calls[index]):
                batch.append(tool_calls[index])
                index += 1
            if len(batch) == 1:
                executed.append((batch[0], self._execute_tool(session_id, batch[0])))
                continue

            results: Dict[str, ToolResult] = {}
            max_workers = min(len(batch), 8)
            executor = concurrent.futures.ThreadPoolExecutor(max_workers=max_workers)
            futures = {executor.submit(self._execute_tool, session_id, call): call for call in batch}
            pending = set(futures)
            try:
                while pending:
                    done, pending = concurrent.futures.wait(
                        pending,
                        timeout=0.05,
                        return_when=concurrent.futures.FIRST_COMPLETED,
                    )
                    self._check_cancel(session_id)
                    for future in done:
                        call = futures[future]
                        results[call.id] = future.result()
            except BaseException:
                for future in pending:
                    future.cancel()
                executor.shutdown(wait=False, cancel_futures=True)
                raise
            else:
                executor.shutdown(wait=True)
            for call in batch:
                executed.append((call, results[call.id]))
        return executed

    def _can_run_parallel(self, call: ToolCall) -> bool:
        if call.name in PARALLEL_SAFE_TOOL_NAMES:
            return True
        if call.name == "session":
            action = str(call.arguments.get("action") or "")
            return action in PARALLEL_SAFE_SESSION_ACTIONS
        return False

    def _child_provider_factory(self) -> Optional[Provider]:
        provider = self.provider_factory()
        if provider is None:
            return FakeProvider()
        return provider

    def _spill_large_tool_output(self, ctx: ToolContext, call: ToolCall, result: ToolResult) -> ToolResult:
        summary = result._text_summary()
        if len(summary) <= MAX_INLINE_TOOL_TEXT:
            return result

        output_dir = ctx.session.artifact_dir / "tool-output"
        output_dir.mkdir(parents=True, exist_ok=True)
        path = output_dir / f"{call.id}_{call.name}.txt"
        path.write_text(summary, encoding="utf-8")
        data: Dict[str, Any] = {"truncated": True, "output_path": str(path)}
        if "ok" in result.data:
            data["ok"] = result.data["ok"]
        for key in ("stderr", "error", "error_type"):
            if key in result.data:
                data[key] = str(result.data[key])[:2000]
        text = summary[:MAX_INLINE_TOOL_TEXT] + f"\n\n[full output saved to {path}]"
        return ToolResult(text=text, data=data, images=result.images)

    def _check_cancel(self, session_id: str) -> None:
        request = self.store.cancel_request(session_id)
        if request is not None:
            raise SessionCancelled(session_id, request["reason"])
