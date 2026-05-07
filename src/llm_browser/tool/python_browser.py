from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import threading
import time
import traceback
import types
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Dict, List, Optional

from llm_browser.harness.api import HelperAPI
from llm_browser.harness.helpers import auto_reload_agent_helpers, install_core_helpers
from llm_browser.harness.skills import autoload_skills, install_skill_loader
from llm_browser.session.cancel import SessionCancelled
from llm_browser.tool.browser_exports import install_browser_helpers_module
from llm_browser.tool.context import ToolContext
from llm_browser.tool.python_exec import CancellableTimeModule, cancellable_sleep, cancellation_trace, execute_python, execution_cwd, is_jsonable
from llm_browser.tool.result import ToolImage, ToolResult
from llm_browser.tool.web_fetch import _browser_headers, _fetch_text_with_curl_cffi

if TYPE_CHECKING:
    from llm_browser.browser import BrowserRuntime

RuntimeFactory = Callable[[Path, bool], "BrowserRuntime"]
_GLOBAL_EXEC_LOCK = threading.RLock()


class PythonBrowserTool:
    """Persistent Python execution environment with browser harness helpers."""

    def __init__(self, runtime_factory: Optional[RuntimeFactory] = None) -> None:
        self.runtime_factory = runtime_factory or self._default_runtime_factory
        self._namespaces: Dict[str, Dict[str, Any]] = {}
        self._runtimes: Dict[str, BrowserRuntime] = {}
        self._exec_lock = threading.RLock()

    def __call__(self, ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
        code = str(arguments.get("code", ""))
        if not code.strip():
            raise ValueError("python tool requires non-empty code")

        headless = bool(arguments.get("headless", _env_bool("LLM_BROWSER_HEADLESS", False)))
        images: List[ToolImage] = []
        namespace = self._namespace(ctx, headless=headless, images=images)
        namespace.pop("_result", None)
        namespace.pop("result", None)

        stdout = io.StringIO()
        stderr = io.StringIO()
        runtime = self._runtime(ctx, headless=headless)
        live_url_before = str(getattr(runtime, "cloud_live_url", "") or "")
        previous_cancel_check = getattr(runtime, "cancel_check", None)

        def check_cancel() -> None:
            ctx.check_cancel()

        if hasattr(runtime, "set_cancel_check"):
            runtime.set_cancel_check(check_cancel)
        try:
            with _GLOBAL_EXEC_LOCK:
                with (
                    execution_cwd(ctx.session.cwd, self._exec_lock),
                    contextlib.redirect_stdout(stdout),
                    contextlib.redirect_stderr(stderr),
                    cancellation_trace(check_cancel),
                ):
                    value = execute_python(code, namespace)
        except SessionCancelled:
            raise
        except BaseException:
            err = stderr.getvalue()
            err += traceback.format_exc()
            self._emit_live_preview_if_changed(ctx, runtime, live_url_before)
            return ToolResult(text=stdout.getvalue(), data={"stderr": err, "ok": False}, images=images)
        finally:
            if hasattr(runtime, "set_cancel_check"):
                runtime.set_cancel_check(previous_cancel_check)

        self._emit_live_preview_if_changed(ctx, runtime, live_url_before)
        if value is None:
            value = namespace.get("_result", namespace.get("result"))

        text = stdout.getvalue()
        data: Dict[str, Any] = {"ok": True}
        if stderr.getvalue():
            data["stderr"] = stderr.getvalue()
        if value is not None:
            if is_jsonable(value):
                data["result"] = value
            else:
                data["result_repr"] = repr(value)
        return ToolResult(text=text, data=data, images=images)

    def close_session(self, session_id: str, stop_browser: bool = True) -> None:
        runtime = self._runtimes.pop(session_id, None)
        if runtime is not None:
            runtime.close(stop_browser=stop_browser)
        self._namespaces.pop(session_id, None)

    def _namespace(self, ctx: ToolContext, headless: bool, images: List[ToolImage]) -> Dict[str, Any]:
        namespace = self._namespaces.get(ctx.session.id)
        runtime = self._runtime(ctx, headless=headless)
        if namespace is None:
            namespace = {
                "__name__": "__llm_browser_python__",
                "json": json,
                "os": os,
                "Path": Path,
                "time": time,
                "display": _display,
            }
            _install_optional_imports(namespace)
            self._namespaces[ctx.session.id] = namespace

        def check_cancel() -> None:
            ctx.check_cancel()

        def cancel_requested() -> bool:
            return ctx.is_cancel_requested()

        def sleep(seconds: float) -> None:
            cancellable_sleep(seconds, check_cancel)

        namespace["time"] = CancellableTimeModule(check_cancel)
        api = HelperAPI(
            ctx=ctx,
            runtime=runtime,
            images=images,
            namespace=namespace,
            check_cancel=check_cancel,
            cancel_requested=cancel_requested,
            sleep=sleep,
        )
        install_core_helpers(api)
        install_skill_loader(api)
        autoload_skills(api)
        install_browser_helpers_module(namespace)
        auto_reload_agent_helpers(api)
        return namespace

    def _runtime(self, ctx: ToolContext, headless: bool) -> "BrowserRuntime":
        runtime = self._runtimes.get(ctx.session.id)
        if runtime is not None:
            return runtime
        root_dir = ctx.session.artifact_dir / "browser"
        runtime = self.runtime_factory(root_dir, headless)
        self._runtimes[ctx.session.id] = runtime
        live_url = str(getattr(runtime, "cloud_live_url", "") or "")
        if live_url:
            ctx.store.emit(
                ctx.session.id,
                "browser.live_url",
                {
                    "mode": str(getattr(runtime, "mode", "") or "cloud"),
                    "browser_id": str(getattr(runtime, "cloud_browser_id", "") or ""),
                    "live_url": live_url,
                },
            )
        return runtime

    def _emit_live_preview_if_changed(self, ctx: ToolContext, runtime: "BrowserRuntime", previous_live_url: str) -> None:
        live_url = str(getattr(runtime, "cloud_live_url", "") or "")
        if not live_url or live_url == previous_live_url:
            return
        ctx.store.emit(
            ctx.session.id,
            "browser.live_url",
            {
                "mode": str(getattr(runtime, "mode", "") or "cloud"),
                "browser_id": str(getattr(runtime, "cloud_browser_id", "") or ""),
                "live_url": live_url,
            },
        )

    def _default_runtime_factory(self, root_dir: Path, headless: bool) -> "BrowserRuntime":
        from llm_browser.browser import BrowserRuntime

        return BrowserRuntime.start(root_dir=root_dir, headless=headless)


def _env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.lower() in {"1", "true", "yes", "on"}


def _install_optional_imports(namespace: Dict[str, Any]) -> None:
    _install_display_shim()
    try:
        import requests

        _install_requests_browser_defaults(requests)
        namespace["requests"] = requests
        session = requests.Session()
        session.headers.update(_browser_headers())
        namespace["http"] = session
    except Exception:
        pass
    try:
        from curl_cffi import requests as curl_requests

        namespace["curl_requests"] = curl_requests
    except Exception:
        pass
    try:
        import pandas as pd

        namespace["pd"] = pd
    except Exception:
        pass
    try:
        from bs4 import BeautifulSoup

        namespace["BeautifulSoup"] = BeautifulSoup
    except Exception:
        pass
    try:
        import pypdf
        from pypdf import PdfReader

        namespace["PdfReader"] = PdfReader
        sys.modules.setdefault("PyPDF2", pypdf)
    except Exception:
        pass
    try:
        from PIL import Image

        namespace["Image"] = Image
    except Exception:
        pass


def _install_requests_browser_defaults(requests_module: Any) -> None:
    request = requests_module.sessions.Session.request
    if getattr(request, "_llm_browser_default_headers", False):
        return

    default_headers = _browser_headers()

    def request_with_browser_defaults(self: Any, method: str, url: str, **kwargs: Any) -> Any:
        headers = dict(kwargs.pop("headers", None) or {})
        for key, value in default_headers.items():
            headers.setdefault(key, value)
        kwargs["headers"] = headers
        return request(self, method, url, **kwargs)

    request_with_browser_defaults._llm_browser_default_headers = True  # type: ignore[attr-defined]
    request_with_browser_defaults._llm_browser_original = request  # type: ignore[attr-defined]
    requests_module.sessions.Session.request = request_with_browser_defaults


def _display(*values: Any, **_: Any) -> None:
    for value in values:
        if hasattr(value, "to_markdown"):
            try:
                print(value.to_markdown())
                continue
            except Exception:
                pass
        if hasattr(value, "to_string"):
            try:
                print(value.to_string())
                continue
            except Exception:
                pass
        if isinstance(value, (dict, list, tuple)):
            try:
                print(json.dumps(value, ensure_ascii=False, indent=2))
                continue
            except TypeError:
                pass
        print(value)


def _install_display_shim() -> None:
    if "IPython.display" in sys.modules:
        return
    try:
        import IPython.display  # noqa: F401

        return
    except Exception:
        pass

    ipython_module = sys.modules.get("IPython")
    if ipython_module is None:
        ipython_module = types.ModuleType("IPython")
        sys.modules["IPython"] = ipython_module
    display_module = types.ModuleType("IPython.display")
    display_module.display = _display
    display_module.Markdown = str
    display_module.HTML = str
    setattr(ipython_module, "display", display_module)
    sys.modules["IPython.display"] = display_module
