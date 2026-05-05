from __future__ import annotations

from typing import Any, Dict

from llm_browser.tool.context import ToolContext
from llm_browser.tool.python_browser import PythonBrowserTool
from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolResult
from llm_browser.tool.spec import ToolSpec


def echo(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    return ToolResult(text=str(arguments.get("text", "")))


def done(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    return ToolResult(text=str(arguments.get("result", "")))


def build_builtin_registry() -> ToolRegistry:
    registry = ToolRegistry()
    registry.register(
        ToolSpec(
            name="python",
            description=(
                "Run persistent Python for browser work. Raw CDP is available as "
                "cdp(method, params=None). Helpers include new_tab(url), js(expr), "
                "wait_for_load(), screenshot(label, attach=True), click_at(x,y), "
                "type_text(text), press(key), scroll(dx=0, dy=500), and page_info(). "
                "Set result or _result for structured output."
            ),
            input_schema={
                "type": "object",
                "properties": {
                    "code": {"type": "string"},
                    "headless": {"type": "boolean"},
                },
                "required": ["code"],
                "additionalProperties": False,
            },
        ),
        PythonBrowserTool(),
    )
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
