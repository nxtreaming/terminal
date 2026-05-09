from __future__ import annotations

import hashlib
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from llm_browser.browser import daemon_ipc as ipc
from llm_browser.browser.runtime import BrowserRuntimeOptions
from llm_browser.tool.result import ToolImage


class DaemonBrowserRuntime:
    def __init__(self, name: str, root_dir: Path, headless: bool = False, backend: str = "chromium") -> None:
        self.name = ipc.normalize_name(name)
        self.root_dir = root_dir
        self.headless = headless
        self.backend = backend
        self.mode = "daemon"
        self.downloads_dir = root_dir / "runtime" / "downloads"

    @classmethod
    def start(cls, root_dir: Path, headless: bool = False, options: Optional[BrowserRuntimeOptions] = None) -> "DaemonBrowserRuntime":
        options = options or BrowserRuntimeOptions.from_env()
        name = options.daemon_name or _default_daemon_name(root_dir)
        backend = options.daemon_backend or _default_backend(options)
        ensure_daemon(name=name, root_dir=root_dir / "daemon", headless=headless, backend=backend)
        return cls(name=name, root_dir=root_dir / "daemon", headless=headless, backend=backend)

    def close(self, stop_browser: bool = True) -> None:
        if stop_browser:
            request(self.name, {"meta": "shutdown"}, timeout_s=10)

    def cdp(
        self,
        method: str,
        params: Optional[Dict[str, Any]] = None,
        session_id: Optional[str] = None,
        timeout_s: Optional[float] = None,
        timeout: Optional[float] = None,
    ) -> Dict[str, Any]:
        if timeout is not None:
            timeout_s = timeout
        payload = {
            "op": "cdp",
            "method": method,
            "params": params or {},
            "session_id": session_id,
            "timeout_s": timeout_s,
        }
        request_timeout_s = max(float(timeout_s or 30), 30)
        response = request(self.name, payload, timeout_s=request_timeout_s)
        return response.get("result") or {}

    def connection_info(self) -> Dict[str, Any]:
        return self._call("connection_info")

    def version(self) -> Dict[str, Any]:
        return self._call("version")

    def targets(self) -> List[Dict[str, Any]]:
        return self._call("targets")

    def tabs(self) -> List[Dict[str, Any]]:
        return self._call("tabs")

    def list_tabs(self, include_internal: bool = True) -> List[Dict[str, Any]]:
        return self._call("list_tabs", include_internal)

    def attach_tab(self, target_id: Optional[str] = None, index: Optional[int] = None, url_contains: Optional[str] = None) -> Dict[str, Any]:
        return self._call("attach_tab", target_id=target_id, index=index, url_contains=url_contains)

    def switch_tab(self, target: Any) -> Dict[str, Any]:
        return self._call("switch_tab", target)

    def current_tab(self) -> Dict[str, Any]:
        return self._call("current_tab")

    def current_cdp_session(self) -> Dict[str, Any]:
        return self._call("current_cdp_session")

    def set_cdp_session(self, session_id: Optional[str], target_id: Optional[str] = None) -> Dict[str, Any]:
        return self._call("set_cdp_session", session_id, target_id=target_id)

    def reattach_cdp(
        self,
        target_id: Optional[str] = None,
        url_contains: Optional[str] = None,
        index: int = 0,
        include_internal: bool = False,
    ) -> Dict[str, Any]:
        return self._call(
            "reattach_cdp",
            target_id=target_id,
            url_contains=url_contains,
            index=index,
            include_internal=include_internal,
        )

    def ensure_real_tab(self) -> Optional[Dict[str, Any]]:
        return self._call("ensure_real_tab")

    def iframe_target(self, url_substr: str) -> Optional[str]:
        return self._call("iframe_target", url_substr)

    def new_tab(self, url: str = "about:blank") -> Dict[str, Any]:
        return self._call("new_tab", url)

    def navigate(self, url: str, wait: bool = True, timeout_s: float = 20.0) -> Dict[str, Any]:
        return self._call("navigate", url, wait=wait, timeout_s=timeout_s)

    def js(
        self,
        expression: str,
        await_promise: bool = False,
        repl_mode: Optional[bool] = None,
        user_gesture: bool = False,
        timeout_s: Optional[float] = None,
        timeout: Optional[float] = None,
    ) -> Any:
        if timeout is not None:
            timeout_s = timeout
        return self._call(
            "js",
            expression,
            await_promise=await_promise,
            repl_mode=repl_mode,
            user_gesture=user_gesture,
            timeout_s=timeout_s,
        )

    def wait_for_load(self, timeout_s: float = 20.0) -> None:
        self._call("wait_for_load", timeout_s=timeout_s)

    def wait_until(self, expression: str, timeout_s: float = 20.0, interval_s: float = 0.25) -> Any:
        return self._call("wait_until", expression, timeout_s=timeout_s, interval_s=interval_s)

    def wait_for_selector(self, selector: str, timeout_s: float = 20.0, visible: bool = False) -> Any:
        return self._call("wait_for_selector", selector, timeout_s=timeout_s, visible=visible)

    def wait_for_text(self, text: str, timeout_s: float = 20.0) -> Any:
        return self._call("wait_for_text", text, timeout_s=timeout_s)

    def wait_for_network_idle(
        self,
        timeout_s: float = 10.0,
        idle_ms: int = 500,
        timeout: Optional[float] = None,
    ) -> bool:
        if timeout is not None:
            timeout_s = timeout
        return bool(self._call("wait_for_network_idle", timeout_s=timeout_s, idle_ms=idle_ms))

    def page_info(self) -> Dict[str, Any]:
        return self._call("page_info")

    def visible_text(self, max_chars: int = 8000) -> str:
        return str(self._call("visible_text", max_chars=max_chars))

    def links(self, limit: int = 200) -> List[Dict[str, str]]:
        return self._call("links", limit=limit)

    def screenshot(
        self,
        label: str = "screenshot",
        attach: bool = True,
        full_page: bool = False,
        timeout_s: float = 8.0,
        clip: Optional[Dict[str, float]] = None,
        timeout: Optional[float] = None,
    ) -> ToolImage:
        if timeout is not None:
            timeout_s = timeout
        data = self._call("screenshot", label, attach=attach, full_page=full_page, timeout_s=timeout_s, clip=clip)
        return ToolImage(**data)

    def click_at(self, x: float, y: float, button: str = "left", clicks: int = 1) -> None:
        self._call("click_at", x, y, button=button, clicks=clicks)

    def fill_input(
        self,
        selector: str,
        text: str,
        clear_first: bool = True,
        timeout_s: float = 0.0,
        timeout: Optional[float] = None,
    ) -> None:
        if timeout is not None:
            timeout_s = timeout
        self._call("fill_input", selector, text, clear_first=clear_first, timeout_s=timeout_s)

    def type_text(self, text: str) -> None:
        self._call("type_text", text)

    def press(self, key: str) -> None:
        self._call("press", key)

    def press_key(self, key: str, modifiers: int = 0) -> None:
        self._call("press_key", key, modifiers=modifiers)

    def scroll(self, dx: float = 0, dy: float = 500, x: float = 500, y: float = 500) -> None:
        self._call("scroll", dx=dx, dy=dy, x=x, y=y)

    def drain_events(self, timeout_s: float = 0.05, max_events: int = 1000) -> List[Dict[str, Any]]:
        return self._call("drain_events", timeout_s=timeout_s, max_events=max_events)

    def pending_dialog_info(self, drain: bool = True) -> Optional[Dict[str, Any]]:
        return self._call("pending_dialog_info", drain=drain)

    def recent_cdp_events(
        self,
        prefix: Optional[str] = None,
        limit: int = 100,
        drain: bool = True,
        timeout_s: float = 0.02,
    ) -> List[Dict[str, Any]]:
        return self._call("recent_cdp_events", prefix=prefix, limit=limit, drain=drain, timeout_s=timeout_s)

    def recent_console_events(self, limit: int = 50, drain: bool = True) -> List[Dict[str, Any]]:
        return self._call("recent_console_events", limit=limit, drain=drain)

    def recent_network_events(self, limit: int = 100, drain: bool = True) -> List[Dict[str, Any]]:
        return self._call("recent_network_events", limit=limit, drain=drain)

    def recent_network_failures(self, limit: int = 50, drain: bool = True) -> List[Dict[str, Any]]:
        return self._call("recent_network_failures", limit=limit, drain=drain)

    def download_info(self, limit: int = 100, drain: bool = True) -> Dict[str, Any]:
        return self._call("download_info", limit=limit, drain=drain)

    def save_browser_trace(self, label: str = "browser_trace", include_history: bool = True) -> Dict[str, Any]:
        return self._call("save_browser_trace", label=label, include_history=include_history)

    def _call(self, name: str, *args: Any, **kwargs: Any) -> Any:
        payload = {"op": "call", "name": name, "args": list(args), "kwargs": kwargs}
        timeout_s = max(float(kwargs.get("timeout_s", 30) or 30), 30)
        response = request(self.name, payload, timeout_s=timeout_s)
        return response.get("result")


def request(name: str, payload: Dict[str, Any], timeout_s: float = 30.0) -> Dict[str, Any]:
    return ipc.request(name, payload, timeout_s=timeout_s)


def ensure_daemon(name: str, root_dir: Path, headless: bool, backend: str, wait_s: float = 20.0) -> None:
    existing = _daemon_status_payload(name)
    if existing is not None:
        if _daemon_matches(existing, root_dir=root_dir, headless=headless, backend=backend):
            return
        stop_daemon(name, timeout_s=5)
    ipc.cleanup_stale(name)
    existing = _daemon_status_payload(name)
    if existing is not None and _daemon_matches(existing, root_dir=root_dir, headless=headless, backend=backend):
        return
    root_dir.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["LLM_BROWSER_MODE"] = backend
    env["LLM_BROWSER_DAEMON_BACKEND"] = backend
    command = [
        sys.executable,
        "-m",
        "llm_browser.browser.daemon",
        "--name",
        name,
        "--root-dir",
        str(root_dir),
        "--backend",
        backend,
    ]
    if headless:
        command.append("--headless")
    log = ipc.log_path(name).open("ab")
    process = subprocess.Popen(command, env=env, stdout=log, stderr=log, **ipc.spawn_kwargs())
    ipc.pid_path(name).write_text(str(process.pid), encoding="utf-8")
    deadline = time.time() + wait_s
    while time.time() < deadline:
        status = _daemon_status_payload(name, timeout_s=0.5)
        pid = status.get("pid") if status else None
        if status is not None and _daemon_matches(status, root_dir=root_dir, headless=headless, backend=backend) and (
            pid == process.pid or (type(pid) is int and ipc.pid_alive(pid))
        ):
            return
        time.sleep(0.2)
    raise RuntimeError(f"browser daemon {name!r} did not start; see {ipc.log_path(name)}")


def _daemon_status_payload(name: str, timeout_s: float = 1.0) -> Optional[Dict[str, Any]]:
    try:
        status = ipc.request(name, {"meta": "status"}, timeout_s=timeout_s)
    except Exception:
        return None
    return status if isinstance(status, dict) and status.get("ok") else None


def _daemon_matches(status: Dict[str, Any], *, root_dir: Path, headless: bool, backend: str) -> bool:
    try:
        status_root = Path(str(status.get("root_dir") or "")).expanduser().resolve()
        expected_root = root_dir.expanduser().resolve()
    except Exception:
        return False
    return (
        status_root == expected_root
        and bool(status.get("headless")) == bool(headless)
        and str(status.get("backend") or "") == str(backend or "")
    )


def stop_daemon(name: str, timeout_s: float = 10.0) -> bool:
    if not ipc.ping(name):
        ipc.cleanup(name)
        return False
    try:
        ipc.request(name, {"meta": "shutdown"}, timeout_s=timeout_s)
    finally:
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            if not ipc.ping(name):
                ipc.cleanup(name)
                return True
            time.sleep(0.2)
    return True


def daemon_status(name: str) -> Dict[str, Any]:
    if not ipc.ping(name):
        pid = ipc.read_pid(name)
        return {
            "ok": False,
            "name": name,
            "endpoint": ipc.endpoint(name),
            "alive": False,
            "pid": pid,
            "pid_alive": ipc.pid_alive(pid),
            "log": str(ipc.log_path(name)),
        }
    status = ipc.request(name, {"meta": "status"}, timeout_s=5)
    status["alive"] = True
    status["pid_alive"] = ipc.pid_alive(status.get("pid") if isinstance(status.get("pid"), int) else None)
    status["log"] = str(ipc.log_path(name))
    return status


def _default_daemon_name(root_dir: Path) -> str:
    digest = hashlib.sha1(str(root_dir.resolve()).encode("utf-8")).hexdigest()[:12]
    return f"session-{digest}"


def _default_backend(options: BrowserRuntimeOptions) -> str:
    if options.cdp_http_url or options.cdp_ws_url:
        return "cdp"
    if options.cloud_api_key:
        return "cloud"
    return "chromium"
