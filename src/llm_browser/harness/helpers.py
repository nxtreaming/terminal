from __future__ import annotations

import json
import shutil
import sys
import types
from pathlib import Path
from typing import Any, Dict, Optional

from llm_browser.harness.api import HelperAPI
from llm_browser.tool.result import ToolImage


PRIMARY_CORE_HELPERS = [
    "cdp",
    "js",
    "new_tab",
    "goto_url",
    "page_info",
    "capture_screenshot",
    "click_at_xy",
    "type_text",
    "fill_input",
    "press_key",
    "scroll",
    "wait_for_load",
    "wait_for_element",
    "wait_for_network_idle",
    "http_get",
    "list_tabs",
    "current_tab",
    "switch_tab",
    "ensure_real_tab",
    "output_path",
    "agent_helpers_path",
    "reload_agent_helpers",
]

COMPAT_CORE_HELPERS = [
    "navigate",
    "tabs",
    "attach_tab",
    "screenshot",
    "click_at",
    "press",
    "wait_for_selector",
    "wait_for_text",
    "iframe_target",
    "sleep",
    "check_cancel",
    "cancel_requested",
    "browser",
    "artifact_dir",
    "download_dir",
    "cwd",
    "workspace_dir",
    "output_dir",
]

CORE_HELPERS = PRIMARY_CORE_HELPERS + COMPAT_CORE_HELPERS


def install_core_helpers(api: HelperAPI) -> Dict[str, Any]:
    runtime = api.runtime
    namespace = api.namespace

    def cdp(
        method: str,
        params: Optional[Dict[str, Any]] = None,
        session_id: Optional[str] = None,
        timeout_s: Optional[float] = None,
        timeout: Optional[float] = None,
        retry: bool = True,
        **kwargs: Any,
    ) -> Dict[str, Any]:
        if timeout is not None:
            timeout_s = timeout
        api.check_cancel()
        if params is not None and not isinstance(params, dict):
            raise TypeError("cdp params must be a dict when provided")
        merged_params = dict(params or {})
        merged_params.update(kwargs)
        return runtime.cdp(method, params=merged_params, session_id=session_id, timeout_s=timeout_s, retry=retry)

    def new_tab(url: str = "about:blank") -> Dict[str, Any]:
        api.check_cancel()
        return runtime.new_tab(url)

    def navigate(url: str, wait: bool = True, timeout_s: float = 20.0, timeout: Optional[float] = None) -> Dict[str, Any]:
        if timeout is not None:
            timeout_s = timeout
        api.check_cancel()
        return runtime.navigate(url, wait=wait, timeout_s=timeout_s)

    def goto_url(url: str, wait: bool = True, timeout_s: float = 20.0, timeout: Optional[float] = None) -> Dict[str, Any]:
        return navigate(url, wait=wait, timeout_s=timeout_s, timeout=timeout)

    def js(
        expression: str,
        await_promise: bool = True,
        repl_mode: Optional[bool] = None,
        user_gesture: bool = False,
        timeout_s: Optional[float] = None,
        timeout: Optional[float] = None,
    ) -> Any:
        if timeout is not None:
            timeout_s = timeout
        api.check_cancel()
        return runtime.js(
            expression,
            await_promise=await_promise,
            repl_mode=repl_mode,
            user_gesture=user_gesture,
            timeout_s=timeout_s,
        )

    def wait_for_load(timeout_s: float = 20.0, timeout: Optional[float] = None) -> None:
        if timeout is not None:
            timeout_s = timeout
        api.check_cancel()
        runtime.wait_for_load(timeout_s=timeout_s)

    def wait_for_selector(
        selector: str,
        timeout_s: float = 20.0,
        timeout: Optional[float] = None,
        visible: bool = False,
    ) -> Any:
        if timeout is not None:
            timeout_s = timeout
        api.check_cancel()
        return runtime.wait_for_selector(selector, timeout_s=timeout_s, visible=visible)

    def wait_for_element(
        selector: str,
        timeout: float = 10.0,
        visible: bool = False,
        timeout_s: Optional[float] = None,
    ) -> Any:
        return wait_for_selector(selector, timeout_s=timeout if timeout_s is None else timeout_s, visible=visible)

    def wait_for_text(text: str, timeout_s: float = 20.0, timeout: Optional[float] = None) -> Any:
        if timeout is not None:
            timeout_s = timeout
        api.check_cancel()
        return runtime.wait_for_text(text, timeout_s=timeout_s)

    def wait_for_network_idle(timeout_s: float = 10.0, timeout: Optional[float] = None, idle_ms: int = 500) -> bool:
        if timeout is not None:
            timeout_s = timeout
        api.check_cancel()
        handler = getattr(runtime, "wait_for_network_idle", None)
        if handler is None:
            return False
        return bool(handler(timeout_s=timeout_s, idle_ms=idle_ms))

    def screenshot(
        label: str = "screenshot",
        attach: bool = True,
        full_page: bool = False,
        timeout_s: float = 8.0,
        timeout: Optional[float] = None,
    ) -> ToolImage:
        if timeout is not None:
            timeout_s = timeout
        image = runtime.screenshot(label=label, attach=attach, full_page=full_page, timeout_s=timeout_s)
        if attach:
            api.emit_image(image)
        return image

    def capture_screenshot(
        path: Optional[str] = None,
        full: bool = False,
        max_dim: Optional[int] = None,
        attach: bool = True,
        label: Optional[str] = None,
        timeout_s: float = 8.0,
        timeout: Optional[float] = None,
    ) -> str:
        if timeout is not None:
            timeout_s = timeout
        target_path: Optional[Path] = Path(path).expanduser() if path else None
        if target_path is not None and not target_path.is_absolute():
            target_path = api.cwd / target_path
        image = runtime.screenshot(
            label=label or (target_path.stem if target_path is not None else "screenshot"),
            attach=False,
            full_page=full,
            timeout_s=timeout_s,
        )
        image_path = Path(image.path)
        if target_path is not None:
            target_path.parent.mkdir(parents=True, exist_ok=True)
            if image_path.resolve() != target_path.resolve():
                shutil.copy2(image_path, target_path)
            image_path = target_path
        if max_dim is not None:
            _resize_image_max_dim(image_path, int(max_dim))
        if attach:
            api.emit_image(
                ToolImage(
                    label=label or image.label,
                    path=str(image_path),
                    mime_type=image.mime_type,
                    detail=image.detail,
                    order=image.order,
                    ts_ms=image.ts_ms,
                    url=image.url,
                    title=image.title,
                    viewport=image.viewport,
                )
            )
        return str(image_path)

    def click_at_xy(x: float, y: float, button: str = "left", clicks: int = 1) -> None:
        api.check_cancel()
        return runtime.click_at(x, y, button=button, clicks=clicks)

    def click_at(x: float, y: float, button: str = "left", clicks: int = 1) -> None:
        return click_at_xy(x, y, button=button, clicks=clicks)

    def type_text(text: str) -> None:
        api.check_cancel()
        return runtime.type_text(text)

    def fill_input(*args: Any, **kwargs: Any) -> Any:
        api.check_cancel()
        handler = getattr(runtime, "fill_input", None)
        if handler is None:
            raise RuntimeError("fill_input is unavailable on this runtime")
        return handler(*args, **kwargs)

    def press(key: str) -> None:
        api.check_cancel()
        return runtime.press(key)

    def press_key(key: str, modifiers: int = 0) -> Any:
        api.check_cancel()
        handler = getattr(runtime, "press_key", None)
        if handler is None:
            return runtime.press(key)
        try:
            return handler(key, modifiers=modifiers)
        except TypeError:
            if modifiers:
                raise
            return handler(key)

    def scroll(dx: float = 0, dy: float = 500, x: float = 500, y: float = 500) -> None:
        api.check_cancel()
        return runtime.scroll(dx=dx, dy=dy, x=x, y=y)

    def http_get(url: str, headers: Optional[Dict[str, str]] = None, timeout: float = 20.0) -> str:
        try:
            import requests
        except Exception as exc:
            raise RuntimeError("requests is not installed") from exc
        request_headers = {"User-Agent": "Mozilla/5.0"}
        if headers:
            request_headers.update(headers)
        api.check_cancel()
        response = requests.get(url, headers=request_headers, timeout=timeout)
        api.check_cancel()
        response.raise_for_status()
        return response.text

    def reload_agent_helpers(path: Optional[str] = None) -> Dict[str, Any]:
        helper_path = Path(path).expanduser() if path else api.agent_helpers_path()
        if not helper_path.is_absolute():
            helper_path = api.cwd / helper_path
        code = helper_path.read_text(encoding="utf-8")
        module = types.ModuleType("agent_helpers")
        module.__file__ = str(helper_path)
        exec(compile(code, str(helper_path), "exec"), module.__dict__, module.__dict__)
        sys.modules["agent_helpers"] = module
        explicit_exports = module.__dict__.get("__all__")
        browser_exports = set(getattr(sys.modules.get("browser_helpers"), "__all__", []))
        if explicit_exports is not None:
            export_names = [str(name) for name in explicit_exports]
        else:
            export_names = [
                name
                for name in module.__dict__
                if not name.startswith("_") and name not in browser_exports
            ]
        exported = []
        for name in export_names:
            if name not in module.__dict__ or name.startswith("_"):
                continue
            namespace[name] = module.__dict__[name]
            exported.append(name)
        namespace["_agent_helpers_path"] = str(helper_path)
        namespace["_agent_helpers_loaded_mtime"] = helper_path.stat().st_mtime
        return {"path": str(helper_path), "exports": sorted(exported)}

    def agent_helpers_path() -> str:
        return str(api.agent_helpers_path())

    downloads_dir = api.download_dir
    exports: Dict[str, Any] = {
        "browser": runtime,
        "artifact_dir": api.artifact_dir,
        "download_dir": downloads_dir,
        "cwd": api.cwd,
        "workspace_dir": api.cwd,
        "output_dir": api.output_dir,
        "sleep": api.sleep,
        "output_path": api.output_path,
        "cdp": cdp,
        "new_tab": new_tab,
        "navigate": navigate,
        "goto_url": goto_url,
        "tabs": getattr(runtime, "tabs", lambda: []),
        "attach_tab": getattr(runtime, "attach_tab", lambda *args, **kwargs: None),
        "js": js,
        "wait_for_load": wait_for_load,
        "wait_for_selector": wait_for_selector,
        "wait_for_element": wait_for_element,
        "wait_for_text": wait_for_text,
        "wait_for_network_idle": wait_for_network_idle,
        "screenshot": screenshot,
        "capture_screenshot": capture_screenshot,
        "page_info": getattr(runtime, "page_info", lambda: {}),
        "click_at": click_at,
        "click_at_xy": click_at_xy,
        "fill_input": fill_input,
        "type_text": type_text,
        "press": press,
        "press_key": press_key,
        "scroll": scroll,
        "http_get": http_get,
        "list_tabs": getattr(runtime, "list_tabs", getattr(runtime, "tabs", lambda: [])),
        "current_tab": getattr(runtime, "current_tab", lambda: {}),
        "switch_tab": getattr(runtime, "switch_tab", getattr(runtime, "attach_tab", lambda *args, **kwargs: None)),
        "ensure_real_tab": getattr(runtime, "ensure_real_tab", lambda: None),
        "iframe_target": getattr(runtime, "iframe_target", lambda url_substr=None: None),
        "agent_helpers_path": agent_helpers_path,
        "reload_agent_helpers": reload_agent_helpers,
        "check_cancel": api.check_cancel,
        "cancel_requested": api.cancel_requested,
    }
    namespace.update(exports)
    return exports


def auto_reload_agent_helpers(api: HelperAPI) -> None:
    helper_path = api.agent_helpers_path()
    try:
        mtime = helper_path.stat().st_mtime
    except OSError:
        return
    if api.namespace.get("_agent_helpers_loaded_mtime") == mtime:
        return
    reload_agent_helpers = api.namespace.get("reload_agent_helpers")
    if callable(reload_agent_helpers):
        reload_agent_helpers()


def _resize_image_max_dim(path: Path, max_dim: int) -> None:
    if max_dim <= 0:
        return
    try:
        from PIL import Image
    except Exception as exc:
        raise RuntimeError("Pillow is required for capture_screenshot(max_dim=...)") from exc
    with Image.open(path) as image:
        if max(image.size) <= max_dim:
            return
        image.thumbnail((max_dim, max_dim))
        image.save(path)
