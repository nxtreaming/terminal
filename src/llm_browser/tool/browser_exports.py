from __future__ import annotations

import sys
import types
from typing import Any, Dict

from llm_browser.browser.instructions import BROWSER_HELP_PLAYBOOK


BROWSER_TOOL_DESCRIPTION = (
    "Run persistent Python for direct browser control. Default surface is intentionally small: "
    "raw cdp('Domain.method', key=value), js(expr), new_tab/goto_url, screenshots, coordinate clicks, "
    "keyboard/text input, waits, tabs, simple http_get, output_path, agent_helpers_path/reload_agent_helpers, and "
    "load_skill/list_skills/read_skill/help_browser. Specialized helpers are opt-in with load_skill(name). "
    "Set result or _result for structured output."
)


BROWSER_HELP_TEXT = (
    "Browser Python harness quick reference\n\n"
    + BROWSER_HELP_PLAYBOOK.rstrip()
    + """

Core browser:
  cdp(method, params=None, timeout_s=None, retry=True) or cdp("Page.navigate", url="...", timeout=30)
  check_cancel(), cancel_requested()
  js(expr, await_promise=True, repl_mode=None, timeout_s=None) or js(expr, timeout=30)
  new_tab(url), navigate(url), goto_url(url), tabs(), list_tabs(include_internal=True)
  switch_tab(target), current_tab(), ensure_real_tab(), iframe_target(url_substr)

Waiting and observation:
  wait_for_load(), wait_for_selector(selector, visible=False), wait_for_element(selector), wait_for_text(text)
  wait_for_network_idle(timeout_s=10, idle_ms=500)
  page_info()
  http_get(url)

Input:
  click_at(x, y), click_at_xy(x, y), fill_input(selector, text), type_text(text)
  press(key), press_key(key, modifiers=0), scroll(dx=0, dy=500)

Images:
  screenshot(label, attach=True, timeout=8), capture_screenshot(path=None, attach=True, timeout=8)
  output_path(path='')

Skills:
  list_skills(), load_skill(name), read_skill(name), loaded_skills()
  Python skills are opt-in. Examples: load_skill("downloads"), load_skill("research"), load_skill("search").
  Interaction skills are markdown playbooks. Example: read_skill("iframes").

Editable helpers:
  Path(agent_helpers_path()).write_text(...)
  reload_agent_helpers()
  from browser_helpers import *

Example:
  new_tab("https://example.com")
  wait_for_load()
  screenshot("loaded", attach=True)
  result = {"title": js("document.title"), "page": page_info()}
"""
)


CORE_EXPORT_NAMES = [
    "artifact_dir",
    "download_dir",
    "cwd",
    "workspace_dir",
    "output_dir",
    "output_path",
    "cdp",
    "check_cancel",
    "cancel_requested",
    "sleep",
    "load_skill",
    "list_skills",
    "read_skill",
    "loaded_skills",
    "new_tab",
    "navigate",
    "goto_url",
    "tabs",
    "attach_tab",
    "js",
    "wait_for_load",
    "wait_for_selector",
    "wait_for_element",
    "wait_for_text",
    "wait_for_network_idle",
    "http_get",
    "screenshot",
    "capture_screenshot",
    "page_info",
    "click_at",
    "click_at_xy",
    "fill_input",
    "type_text",
    "press",
    "press_key",
    "scroll",
    "list_tabs",
    "current_tab",
    "switch_tab",
    "ensure_real_tab",
    "iframe_target",
    "agent_helpers_path",
    "reload_agent_helpers",
    "help_browser",
]

PYTHON_AFFORDANCE_EXPORT_NAMES = [
    "requests",
    "http",
    "curl_requests",
    "BeautifulSoup",
    "pd",
    "PdfReader",
    "Image",
    "Path",
    "json",
    "os",
    "time",
]


def help_browser() -> str:
    return BROWSER_HELP_TEXT


def install_browser_helpers_module(namespace: Dict[str, Any]) -> None:
    module = types.ModuleType("browser_helpers")
    export_names = _browser_helper_export_names(namespace)
    for name in export_names:
        if name in namespace:
            setattr(module, name, namespace[name])

    structured_fetch_text = namespace.get("fetch_text")
    if callable(structured_fetch_text):
        setattr(module, "fetch_text_result", structured_fetch_text)

        def fetch_text(*args: Any, **kwargs: Any) -> str:
            result = structured_fetch_text(*args, **kwargs)
            if isinstance(result, dict):
                return str(result.get("text") or "")
            return str(result or "")

        setattr(module, "fetch_text", fetch_text)
        setattr(module, "read_url", fetch_text)
        export_names.extend(["fetch_text", "fetch_text_result", "read_url"])

    structured_fetch_readable_text = namespace.get("fetch_readable_text")
    if callable(structured_fetch_readable_text):
        setattr(module, "fetch_readable_text_result", structured_fetch_readable_text)

        def fetch_readable_text(*args: Any, **kwargs: Any) -> str:
            result = structured_fetch_readable_text(*args, **kwargs)
            if isinstance(result, dict):
                return str(result.get("text") or "")
            return str(result or "")

        setattr(module, "fetch_readable_text", fetch_readable_text)
        setattr(module, "readable_text", fetch_readable_text)
        export_names.extend(["fetch_readable_text", "fetch_readable_text_result", "readable_text"])

    structured_search_web = namespace.get("search_web")
    if callable(structured_search_web):
        class SearchResult(dict):
            def __getitem__(self, key: Any) -> Any:
                if isinstance(key, slice):
                    return str(dict(self))[key]
                return super().__getitem__(key)

        setattr(module, "search_web_result", structured_search_web)

        def search_web(*args: Any, **kwargs: Any) -> SearchResult:
            result = structured_search_web(*args, **kwargs)
            if isinstance(result, dict):
                return SearchResult(result)
            return SearchResult({"results": result})

        setattr(module, "search_web", search_web)
        export_names.extend(["search_web", "search_web_result"])

    module.help_browser = namespace.get("help_browser", help_browser)
    module.__all__ = [name for name in export_names if hasattr(module, name)]
    sys.modules["browser_helpers"] = module
    sys.modules["browser_use"] = module
    sys.modules["browser_tools"] = module


def _browser_helper_export_names(namespace: Dict[str, Any]) -> list[str]:
    names = list(CORE_EXPORT_NAMES)
    loaded = namespace.get("_loaded_browser_skills") or {}
    if isinstance(loaded, dict):
        for meta in loaded.values():
            if not isinstance(meta, dict):
                continue
            for export in meta.get("exports", []):
                if isinstance(export, str):
                    names.append(export)
    names.extend(PYTHON_AFFORDANCE_EXPORT_NAMES)
    seen = set()
    unique = []
    for name in names:
        if name in seen:
            continue
        seen.add(name)
        unique.append(name)
    return unique
