from __future__ import annotations

from pathlib import Path
from typing import Any, Dict

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
            description=(
                "Run persistent Python for browser work. Raw CDP is available as "
                "cdp(method, params=None, timeout_s=None, retry=True). Helpers include new_tab(url), navigate(url), tabs(), "
                "attach_tab(...), list_tabs(include_internal=True), switch_tab(target), current_tab(), ensure_real_tab(), iframe_target(url_substr), "
                "js(expr), wait_for_load(), wait_for_network_idle(), screenshot(label, attach=True, timeout_s=8), "
                "screenshot_element(selector, label=None, attach=True, padding=8), "
                "click_at(x,y), fill_input(selector,text), type_text(text), press(key), press_key(key, modifiers=0), scroll(dx=0, dy=500), page_info(), pending_dialog(), "
                "drain_cdp_events(), recent_cdp_events(), recent_console(), recent_network(), recent_network_failures(), "
                "download_info(), save_browser_trace(label='browser_trace'), "
                "visible_text(), deep_text(), links(), click_text(text_or_regex), dismiss_cookie_banners(), "
                "agent_helpers_path(), reload_agent_helpers(), save_helper(), load_helper(), save_artifact(), "
                "upload_artifact(path), create_download_url(path), download_file(url, path=None), "
                "output_path(path=''), read_pdf_text(path_or_url, max_pages=None), "
                "fetch_text(url, use_jina='auto'), fetch_readable_text(url) for cleaned page text, html_to_text(markup), "
                "fetch_many_text(urls, max_workers=8, save_to=None, requests_per_minute=None), "
                "search_web(query, save_raw='auto', include_specialized='auto') with multi-engine parsing and scholarly fallbacks, "
                "extract_links(text, pattern=None), extract_markdown_link_blocks(text, url_pattern=None), "
                "extract_emails(text, domains=None), crawl_site(url, max_pages=12, purpose='contact') for bounded contact/about/team crawling, "
                "extract_store_locator_locations(url_or_interface, save_to=None) returns a dict with locations/count/sample for Bullseye-style store locator JSON lists, "
                "and read_sitemap(url, include=None) for large XML or markdown URL lists. "
                "You can also import these helpers with from browser_helpers import * or from browser_use import *; "
                "those imports make fetch_text(...) and fetch_readable_text(...) return strings, with fetch_text_result(...) "
                "and fetch_readable_text_result(...) available for metadata. "
                "requests, a browser-header http session, curl_requests from curl_cffi, BeautifulSoup, pandas as pd, and PdfReader are preloaded when available. "
                "requests gets browser-like User-Agent and Accept-Language defaults unless overridden. "
                "PyPDF2 imports are shimmed to pypdf when available. "
                "Use output_path('/home/user/outputs/file.csv') for benchmark output files; it maps that expected path into the workspace outputs directory when needed. "
                "create_download_url(path) uploads to Browser Use cloud when BROWSER_USE_API_KEY is set and otherwise returns a local file URL. "
                "js(expr) awaits promises, uses CDP replMode for ordinary snippets so repeated let/const declarations are recoverable, "
                "and disables replMode automatically for async/fetch/Promise expressions so returned data serializes correctly; "
                "pass await_promise=False, repl_mode=True, or repl_mode=False when you need exact Runtime.evaluate behavior. "
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
