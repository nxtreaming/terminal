from __future__ import annotations

from pathlib import Path
from typing import Any, Dict

from llm_browser.tool.browser_exports import BROWSER_TOOL_DESCRIPTION
from llm_browser.tool.context import ToolContext
from llm_browser.tool.files import apply_patch_file, edit_file, glob_files, grep_files, read_file, write_file
from llm_browser.tool.python_browser import PythonBrowserTool
from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolResult
from llm_browser.tool.shell import shell, shell_poll, shell_start, shell_stdin, shell_stop
from llm_browser.tool.spec import ToolSpec


def echo(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    return ToolResult(text=str(arguments.get("text", "")))


def done(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    path_value = arguments.get("path")
    if path_value:
        path = Path(str(path_value)).expanduser()
        if not path.is_absolute():
            path = ctx.session.cwd / path
        text = path.read_text(encoding="utf-8")
        return ToolResult(text=text, data={"ok": True, "path": str(path), "chars": len(text)})
    return ToolResult(text=str(arguments.get("result", "")))


def build_builtin_registry() -> ToolRegistry:
    registry = ToolRegistry()
    registry.register(
        ToolSpec(
            name="python",
            description=BROWSER_TOOL_DESCRIPTION,
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
            name="shell_start",
            description="Start a long-running shell process in the session working directory and return a process id for polling/stdin/stop. Pass pty=true for interactive terminal programs.",
            input_schema={
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_s": {"type": "number"},
                    "pty": {"type": "boolean"},
                },
                "required": ["command"],
                "additionalProperties": False,
            },
        ),
        shell_start,
    )
    registry.register(
        ToolSpec(
            name="shell_poll",
            description="Poll a process started by shell_start. Returns new output by default; pass all=true for full buffered output.",
            input_schema={
                "type": "object",
                "properties": {
                    "process_id": {"type": "string"},
                    "all": {"type": "boolean"},
                },
                "required": ["process_id"],
                "additionalProperties": False,
            },
        ),
        shell_poll,
    )
    registry.register(
        ToolSpec(
            name="shell_stdin",
            description="Write text to stdin for a process started by shell_start.",
            input_schema={
                "type": "object",
                "properties": {
                    "process_id": {"type": "string"},
                    "text": {"type": "string"},
                },
                "required": ["process_id", "text"],
                "additionalProperties": False,
            },
        ),
        shell_stdin,
    )
    registry.register(
        ToolSpec(
            name="shell_stop",
            description="Stop a process started by shell_start and return buffered output.",
            input_schema={
                "type": "object",
                "properties": {
                    "process_id": {"type": "string"},
                },
                "required": ["process_id"],
                "additionalProperties": False,
            },
        ),
        shell_stop,
    )
    registry.register(
        ToolSpec(
            name="read",
            description="Read a text file or list a directory. Supports char offset/limit or line_offset/line_limit. Refuses binary content clearly.",
            input_schema={
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"},
                    "line_offset": {"type": "integer"},
                    "line_limit": {"type": "integer"},
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
            description="Write a UTF-8 text file. Creates parent directories and returns a unified diff.",
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
            description="Replace exact text in a UTF-8 file. Preserves UTF-8 BOM/newline style and returns a unified diff.",
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
            name="apply_patch",
            description="Apply a unified diff patch in the session working directory using git apply.",
            input_schema={
                "type": "object",
                "properties": {
                    "patch": {"type": "string"},
                    "check": {"type": "boolean"},
                },
                "required": ["patch"],
                "additionalProperties": False,
            },
        ),
        apply_patch_file,
    )
    registry.register(
        ToolSpec(
            name="glob",
            description="List files matching a glob pattern under a root, newest first.",
            input_schema={
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "root": {"type": "string"},
                    "limit": {"type": "integer"},
                    "recursive": {"type": "boolean"},
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
            description="Search text files for a literal pattern using rg when available.",
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
            description=(
                "Finish the current task with a final result. Use result for normal answers. "
                "Use path for a large final text/JSON/CSV answer already saved in the workspace; "
                "the file contents become the final result, not a link."
            ),
            input_schema={
                "type": "object",
                "properties": {"result": {"type": "string"}, "path": {"type": "string"}},
                "additionalProperties": False,
            },
        ),
        done,
    )
    return registry
