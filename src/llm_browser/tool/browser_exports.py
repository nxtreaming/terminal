from __future__ import annotations

import sys
import types
from typing import Any, Dict


BROWSER_TOOL_DESCRIPTION = (
    "Run persistent Python for browser work. Core primitives: raw cdp(method, params), "
    "js(expr), navigate/new_tab/tabs/switch_tab, click_at/fill_input/press/scroll, "
    "screenshot(..., attach=True), wait_for_load/wait_for_network_idle, page_info, "
    "download_info, recent_console, recent_network_failures, save_browser_trace, "
    "agent_helpers_path/reload_agent_helpers, shell/file tools separately, and done. "
    "Use help_browser() inside Python for the full helper list and examples. "
    "Set result or _result for structured output."
)


BROWSER_HELP_TEXT = """Browser Python tool quick reference

Core CDP/browser:
  cdp(method, params=None, timeout_s=None, retry=True)
  js(expr, await_promise=True, repl_mode=None)
  new_tab(url), navigate(url), tabs(), list_tabs(include_internal=True)
  switch_tab(target), current_tab(), ensure_real_tab(), iframe_target(url_substr)

Waiting and observation:
  wait_for_load(), wait_for_selector(selector, visible=False), wait_for_text(text)
  wait_for_network_idle(timeout_s=10, idle_ms=500)
  page_info(), pending_dialog(), visible_text(), deep_text(), links()
  drain_cdp_events(), recent_console(), recent_network(), recent_network_failures()

Input:
  click_at(x, y), fill_input(selector, text), type_text(text)
  press(key), press_key(key, modifiers=0), scroll(dx=0, dy=500)

Images and artifacts:
  screenshot(label, attach=True), screenshot_element(selector, label=None)
  download_info(), save_browser_trace(label)
  save_artifact(name, content=None), upload_artifact(path), create_download_url(path)
  output_path(path=''), download_file(url, path=None), read_pdf_text(path_or_url)

Editable helpers:
  Path(agent_helpers_path()).write_text(...)
  reload_agent_helpers()
  from browser_helpers import *

Fetch/extract helpers:
  fetch_text(url), fetch_readable_text(url), fetch_many_text(urls)
  search_web(query), crawl_site(url), html_to_text(markup), read_sitemap(url)
  extract_links(text), extract_emails(text), extract_markdown_link_blocks(text)

Example:
  new_tab("https://example.com")
  wait_for_load()
  screenshot("loaded", attach=True)
  result = {"title": js("document.title"), "text": visible_text(1000)}
"""


EXPORT_NAMES = [
    "artifact_dir",
    "download_dir",
    "cwd",
    "workspace_dir",
    "output_dir",
    "output_path",
    "cdp",
    "new_tab",
    "navigate",
    "tabs",
    "attach_tab",
    "js",
    "wait_for_load",
    "wait_until",
    "wait_for_selector",
    "wait_for_text",
    "wait_for_network_idle",
    "deep_text",
    "click_text",
    "dismiss_cookie_banners",
    "screenshot",
    "screenshot_element",
    "page_info",
    "pending_dialog",
    "drain_cdp_events",
    "recent_cdp_events",
    "recent_console",
    "recent_network",
    "recent_network_failures",
    "download_info",
    "save_browser_trace",
    "visible_text",
    "links",
    "click_at",
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
    "load_helper",
    "save_helper",
    "agent_helpers_path",
    "reload_agent_helpers",
    "help_browser",
    "save_artifact",
    "upload_artifact",
    "create_download_url",
    "artifact_download_url",
    "download_file",
    "read_pdf_text",
    "html_to_text",
    "fetch_readable_text",
    "search_web",
    "extract_links",
    "extract_markdown_link_blocks",
    "extract_emails",
    "crawl_site",
    "extract_store_locator_locations",
    "store_locator_locations",
    "read_sitemap",
    "fetch_many_text",
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
    export_names = list(EXPORT_NAMES)
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

    module.help_browser = help_browser
    module.__all__ = [name for name in export_names if hasattr(module, name)]
    sys.modules["browser_helpers"] = module
    sys.modules["browser_use"] = module
    sys.modules["browser_tools"] = module
