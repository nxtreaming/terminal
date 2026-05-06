from __future__ import annotations

import fnmatch
import time
from typing import Any, Callable, Dict, List, Optional
from urllib.parse import urlparse

from llm_browser.tool.python_exec import cancellable_sleep


CancelCheck = Callable[[], None]


def get_cookies(runtime: Any, cancel_check: CancelCheck, urls: Optional[List[str]] = None) -> Dict[str, Any]:
    cancel_check()
    params: Dict[str, Any] = {}
    if urls is not None:
        params["urls"] = [str(url) for url in urls]
    return runtime.cdp("Network.getCookies", params=params, timeout_s=5)


def set_cookie(runtime: Any, cancel_check: CancelCheck, cookie: Optional[Dict[str, Any]] = None, **kwargs: Any) -> Dict[str, Any]:
    params = dict(cookie or {})
    params.update(kwargs)
    if "url" not in params and "domain" not in params:
        page = runtime.page_info()
        url = str(page.get("url") or "")
        if url.startswith(("http://", "https://")):
            params["url"] = url
    if "name" not in params or "value" not in params:
        raise ValueError("set_cookie requires name and value")
    cancel_check()
    return runtime.cdp("Network.setCookie", params=params, timeout_s=5)


def clear_cookies(runtime: Any, cancel_check: CancelCheck) -> Dict[str, Any]:
    cancel_check()
    return runtime.cdp("Network.clearBrowserCookies", timeout_s=5)


def storage_state(runtime: Any, cancel_check: CancelCheck, include_cookies: bool = True) -> Dict[str, Any]:
    cancel_check()
    state = runtime.js(
        """
(() => ({
  url: location.href,
  origin: location.origin,
  localStorage: Object.fromEntries(Object.entries(localStorage)),
  sessionStorage: Object.fromEntries(Object.entries(sessionStorage)),
}))()
        """,
        await_promise=False,
    )
    if not isinstance(state, dict):
        state = {}
    if include_cookies:
        state["cookies"] = get_cookies(runtime, cancel_check).get("cookies", [])
    return state


def clear_storage(runtime: Any, cancel_check: CancelCheck, origin: Optional[str] = None, storage_types: str = "all") -> Dict[str, Any]:
    if origin is None:
        page = runtime.page_info()
        page_url = str(page.get("url") or "")
        parsed = urlparse(page_url)
        if parsed.scheme in {"http", "https"} and parsed.netloc:
            origin = f"{parsed.scheme}://{parsed.netloc}"
    if not origin:
        raise ValueError("clear_storage requires an origin when the current page has no web origin")
    cancel_check()
    return runtime.cdp(
        "Storage.clearDataForOrigin",
        params={"origin": origin, "storageTypes": storage_types},
        timeout_s=5,
    )


def grant_permissions(
    runtime: Any,
    cancel_check: CancelCheck,
    permissions: List[str],
    origin: Optional[str] = None,
    browser_context_id: Optional[str] = None,
) -> Dict[str, Any]:
    params: Dict[str, Any] = {"permissions": [str(permission) for permission in permissions]}
    if origin:
        params["origin"] = origin
    if browser_context_id:
        params["browserContextId"] = browser_context_id
    cancel_check()
    return runtime.cdp("Browser.grantPermissions", params=params, timeout_s=5)


def reset_permissions(runtime: Any, cancel_check: CancelCheck, browser_context_id: Optional[str] = None) -> Dict[str, Any]:
    params: Dict[str, Any] = {}
    if browser_context_id:
        params["browserContextId"] = browser_context_id
    cancel_check()
    return runtime.cdp("Browser.resetPermissions", params=params, timeout_s=5)


def wait_for_download(
    runtime: Any,
    cancel_check: CancelCheck,
    pattern: Optional[str] = None,
    timeout_s: float = 30.0,
    poll_s: float = 0.25,
) -> Dict[str, Any]:
    deadline = time.monotonic() + max(0.0, float(timeout_s))
    while True:
        cancel_check()
        info = getattr(
            runtime,
            "download_info",
            lambda *args, **kwargs: {"downloads_dir": str(getattr(runtime, "downloads_dir", "")), "files": [], "events": []},
        )(drain=True)
        files = info.get("files") if isinstance(info, dict) else []
        for item in files if isinstance(files, list) else []:
            name = str(item.get("name") or item.get("relative_path") or "")
            path = str(item.get("path") or "")
            if pattern and not (fnmatch.fnmatch(name, pattern) or fnmatch.fnmatch(path, pattern)):
                continue
            if item.get("complete") is not False:
                return dict(item)
        if time.monotonic() >= deadline:
            raise TimeoutError(f"download did not complete within {timeout_s:g}s")
        cancellable_sleep(max(0.01, float(poll_s)), cancel_check)
