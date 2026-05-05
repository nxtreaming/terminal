from __future__ import annotations

from typing import Any, Callable, Dict, List

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult
from llm_browser.tool.spec import ToolSpec

ToolHandler = Callable[[ToolContext, Dict[str, Any]], ToolResult]


class ToolRegistry:
    def __init__(self) -> None:
        self._specs: Dict[str, ToolSpec] = {}
        self._handlers: Dict[str, ToolHandler] = {}

    def register(self, spec: ToolSpec, handler: ToolHandler) -> None:
        if spec.name in self._handlers:
            raise ValueError(f"tool already registered: {spec.name}")
        self._specs[spec.name] = spec
        self._handlers[spec.name] = handler

    def specs(self) -> List[Dict[str, Any]]:
        return [spec.to_provider_tool() for spec in self._specs.values()]

    def run(self, name: str, arguments: Dict[str, Any], ctx: ToolContext) -> ToolResult:
        if name not in self._handlers:
            raise KeyError(f"unknown tool: {name}")
        return self._handlers[name](ctx, arguments)

    def close_session(self, session_id: str) -> None:
        for handler in self._handlers.values():
            close = getattr(handler, "close_session", None)
            if callable(close):
                close(session_id)
