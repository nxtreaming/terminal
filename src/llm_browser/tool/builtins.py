from __future__ import annotations

from typing import Any, Dict

from llm_browser.tool.context import ToolContext
from llm_browser.tool.files import edit_file, glob_files, grep_files, read_file, write_file
from llm_browser.tool.python_browser import PythonBrowserTool
from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolResult
from llm_browser.tool.shell import shell
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
            name="shell",
            description="Run a shell command in the session working directory. Large output is saved to an artifact file.",
            input_schema={
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_s": {"type": "number"},
                },
                "required": ["command"],
                "additionalProperties": False,
            },
        ),
        shell,
    )
    registry.register(
        ToolSpec(
            name="read",
            description="Read a UTF-8 text file. Relative paths resolve from the session working directory.",
            input_schema={
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"},
                },
                "required": ["path"],
                "additionalProperties": False,
            },
        ),
        read_file,
    )
    registry.register(
        ToolSpec(
            name="write",
            description="Write a UTF-8 text file. Creates parent directories.",
            input_schema={
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["path", "content"],
                "additionalProperties": False,
            },
        ),
        write_file,
    )
    registry.register(
        ToolSpec(
            name="edit",
            description="Replace exact text in a UTF-8 file. By default old text must appear exactly once.",
            input_schema={
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"},
                    "replace_all": {"type": "boolean"},
                },
                "required": ["path", "old", "new"],
                "additionalProperties": False,
            },
        ),
        edit_file,
    )
    registry.register(
        ToolSpec(
            name="glob",
            description="List files matching a glob pattern under a root.",
            input_schema={
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "root": {"type": "string"},
                    "limit": {"type": "integer"},
                },
                "required": ["pattern"],
                "additionalProperties": False,
            },
        ),
        glob_files,
    )
    registry.register(
        ToolSpec(
            name="grep",
            description="Search UTF-8 text files for a literal pattern.",
            input_schema={
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "root": {"type": "string"},
                    "include": {"type": "string"},
                    "limit": {"type": "integer"},
                },
                "required": ["pattern"],
                "additionalProperties": False,
            },
        ),
        grep_files,
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
