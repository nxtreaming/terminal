from __future__ import annotations

import sys
import types
from typing import Any, Dict

from llm_browser.browser.instructions import BROWSER_HELP_PLAYBOOK


BROWSER_TOOL_DESCRIPTION = (
    "Run Python in a browser-connected environment. "
    "CDP is the source of truth: use cdp(\"Domain.method\", **params) for browser state and debugging. "
    "Helpers such as page_info(), goto_url(), js(), capture_screenshot(), click_at_xy(), fill_input(), and press_key() "
    "are Python functions inside this tool's code string and convenience wrappers you may use when clearer. "
    "The harness owns Chrome lifecycle and the active CDP connection; "
    "avoid raw localhost DevTools discovery URLs or relaunching Chrome. "
    "Set result or _result for structured output."
)

LAZY_IMPORT_EXPORT_NAMES = {
    "click_text",
    "deep_text",
    "dismiss_cookie_banners",
    "screenshot_element",
}


BROWSER_HELP_TEXT = (
    "Browser Python harness quick reference\n\n"
    + BROWSER_HELP_PLAYBOOK.rstrip()
    + """

Core browser:
  cdp(method, params=None, timeout_s=None) or cdp("Page.navigate", url="...", timeout=30)
  new_tab(url), goto_url(url), list_tabs(include_internal=True)
  switch_tab(target), current_tab(), current_cdp_session(), set_cdp_session(session_id, target_id=None), reattach_cdp(...)
  ensure_real_tab()
  js(expr, await_promise=True, repl_mode=None, timeout_s=None) or js(expr, timeout=30)

Waiting and observation:
  wait_for_load(), wait_for_element(selector)
  wait_for_network_idle(timeout_s=10, idle_ms=500)
  page_info(), pending_dialog(), handle_dialog(accept=True, prompt_text=None)
  http_get(url)

Input:
  click_at_xy(x, y), fill_input(selector, text), type_text(text)
  press_key(key, modifiers=0), scroll(dx=0, dy=500)

Images:
  capture_screenshot(path=None, attach=True, timeout=8)
  output_path(path='')

Skills:
  list_skills(), load_skill(name), read_skill(name), loaded_skills()
  Python skills are opt-in. Examples: load_skill("downloads"), load_skill("research"), load_skill("search").
  Interaction skills are markdown playbooks. Example: read_skill("iframes").

Editable helpers:
  Path(agent_helpers_path()).write_text(...)
  reload_agent_helpers()
  from browser import *  # browser_helpers/browser_use/browser_tools are aliases

Example:
  result = cdp("Browser.getVersion")
"""
)


PRIMARY_CORE_EXPORT_NAMES = [
    "cdp",
    "load_skill",
    "list_skills",
    "read_skill",
    "loaded_skills",
    "help_browser",
    "new_tab",
    "goto_url",
    "js",
    "wait_for_load",
    "wait_for_element",
    "wait_for_network_idle",
    "http_get",
    "capture_screenshot",
    "page_info",
    "pending_dialog",
    "handle_dialog",
    "click_at_xy",
    "fill_input",
    "type_text",
    "press_key",
    "scroll",
    "list_tabs",
    "current_tab",
    "switch_tab",
    "current_cdp_session",
    "set_cdp_session",
    "reattach_cdp",
    "ensure_real_tab",
    "output_path",
    "agent_helpers_path",
    "reload_agent_helpers",
]

COMPAT_CORE_EXPORT_NAMES = [
    "artifact_dir",
    "download_dir",
    "cwd",
    "workspace_dir",
    "output_dir",
    "check_cancel",
    "cancel_requested",
    "sleep",
    "browser",
    "navigate",
    "tabs",
    "attach_tab",
    "screenshot",
    "click_at",
    "coordinate_click",
    "press",
    "wait_for_selector",
    "wait_for_text",
    "iframe_target",
]

CORE_EXPORT_NAMES = PRIMARY_CORE_EXPORT_NAMES + COMPAT_CORE_EXPORT_NAMES

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
    attribute_names = _browser_helper_export_names(namespace, include_compat=True)
    star_names = _browser_helper_export_names(namespace, include_compat=False)
    for name in attribute_names:
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
        star_names.extend(["fetch_text", "fetch_text_result", "read_url"])

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
        star_names.extend(["fetch_readable_text", "fetch_readable_text_result", "readable_text"])

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
        star_names.extend(["search_web", "search_web_result"])

    lazy_exports = _lazy_skill_export_map(namespace)

    def __getattr__(name: str) -> Any:
        skill_name = lazy_exports.get(name)
        load_skill = namespace.get("load_skill")
        if skill_name and callable(load_skill):
            load_skill(skill_name)
            if name in namespace:
                return namespace[name]
        raise AttributeError(f"module 'browser_helpers' has no attribute {name!r}")

    module.__getattr__ = __getattr__  # type: ignore[attr-defined]
    module.help_browser = namespace.get("help_browser", help_browser)
    module.__all__ = _unique_names([name for name in star_names if hasattr(module, name)])
    sys.modules["browser_helpers"] = module
    sys.modules["browser"] = module
    sys.modules["browser_use"] = module
    sys.modules["browser_tools"] = module
    sys.modules["agent_browser"] = module


def _browser_helper_export_names(namespace: Dict[str, Any], include_compat: bool) -> list[str]:
    names = list(CORE_EXPORT_NAMES if include_compat else PRIMARY_CORE_EXPORT_NAMES)
    loaded = namespace.get("_loaded_browser_skills") or {}
    if isinstance(loaded, dict):
        for meta in loaded.values():
            if not isinstance(meta, dict):
                continue
            for export in meta.get("exports", []):
                if isinstance(export, str):
                    names.append(export)
    names.extend(PYTHON_AFFORDANCE_EXPORT_NAMES)
    return _unique_names(names)


def _lazy_skill_export_map(namespace: Dict[str, Any]) -> Dict[str, str]:
    list_skills = namespace.get("list_skills")
    if not callable(list_skills):
        return {}
    exports: Dict[str, str] = {}
    try:
        skills = list_skills()
    except Exception:
        return {}
    for skill in skills:
        if not isinstance(skill, dict) or skill.get("kind") != "python":
            continue
        name = str(skill.get("name") or "")
        for export in skill.get("exports") or []:
            if isinstance(export, str) and export:
                if export in LAZY_IMPORT_EXPORT_NAMES:
                    exports.setdefault(export, name)
    return exports


def _unique_names(names: list[str]) -> list[str]:
    seen = set()
    unique = []
    for name in names:
        if name in seen:
            continue
        seen.add(name)
        unique.append(name)
    return unique
