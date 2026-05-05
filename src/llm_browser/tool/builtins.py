from __future__ import annotations

from typing import Any, Dict

from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolResult
from llm_browser.tool.spec import ToolSpec


def echo(arguments: Dict[str, Any]) -> ToolResult:
    return ToolResult(text=str(arguments.get("text", "")))


def done(arguments: Dict[str, Any]) -> ToolResult:
    return ToolResult(text=str(arguments.get("result", "")))


def build_builtin_registry() -> ToolRegistry:
    registry = ToolRegistry()
    registry.register(
        ToolSpec(
            name="echo",
            description="Echo text. Used only by the fake provider and tests.",
            input_schema={
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": False,
            },
        ),
        echo,
    )
    registry.register(
        ToolSpec(
            name="done",
            description="Finish the current task with a final result.",
            input_schema={
                "type": "object",
                "properties": {"result": {"type": "string"}},
                "required": ["result"],
                "additionalProperties": False,
            },
        ),
        done,
    )
    return registry
