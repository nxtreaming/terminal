from __future__ import annotations

import contextlib
import io
import json
import os
import traceback
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

from llm_browser.browser import BrowserRuntime
from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolImage, ToolResult

RuntimeFactory = Callable[[Path, bool], BrowserRuntime]


class PythonBrowserTool:
    """Persistent Python execution environment with browser helpers."""

    def __init__(self, runtime_factory: Optional[RuntimeFactory] = None) -> None:
        self.runtime_factory = runtime_factory or self._default_runtime_factory
        self._namespaces: Dict[str, Dict[str, Any]] = {}
        self._runtimes: Dict[str, BrowserRuntime] = {}

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
        try:
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                value = self._execute(code, namespace)
        except BaseException:
            err = stderr.getvalue()
            err += traceback.format_exc()
            return ToolResult(text=stdout.getvalue(), data={"stderr": err, "ok": False}, images=images)

        if value is None:
            value = namespace.get("_result", namespace.get("result"))

        text = stdout.getvalue()
        data: Dict[str, Any] = {"ok": True}
        if stderr.getvalue():
            data["stderr"] = stderr.getvalue()
        if value is not None:
            if _is_jsonable(value):
                data["result"] = value
            else:
                data["result_repr"] = repr(value)
        return ToolResult(text=text, data=data, images=images)

    def close_session(self, session_id: str) -> None:
        runtime = self._runtimes.pop(session_id, None)
        if runtime is not None:
            runtime.close()
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
            }
            self._namespaces[ctx.session.id] = namespace

        def cdp(
            method: str,
            params: Optional[Dict[str, Any]] = None,
            session_id: Optional[str] = None,
        ) -> Dict[str, Any]:
            return runtime.cdp(method, params=params, session_id=session_id)

        def new_tab(url: str = "about:blank") -> Dict[str, Any]:
            return runtime.new_tab(url)

        def js(expression: str, await_promise: bool = False) -> Any:
            return runtime.js(expression, await_promise=await_promise)

        def wait_for_load(timeout_s: float = 20.0) -> None:
            runtime.wait_for_load(timeout_s=timeout_s)

        def screenshot(label: str = "screenshot", attach: bool = True, full_page: bool = False) -> ToolImage:
            image = runtime.screenshot(label=label, attach=attach, full_page=full_page)
            if attach:
                images.append(image)
                ctx.emit_image(image)
            return image

        namespace.update(
            {
                "browser": runtime,
                "cdp": cdp,
                "new_tab": new_tab,
                "js": js,
                "wait_for_load": wait_for_load,
                "screenshot": screenshot,
                "page_info": runtime.page_info,
                "click_at": runtime.click_at,
                "type_text": runtime.type_text,
                "press": runtime.press,
                "scroll": runtime.scroll,
            }
        )
        return namespace

    def _runtime(self, ctx: ToolContext, headless: bool) -> BrowserRuntime:
        runtime = self._runtimes.get(ctx.session.id)
        if runtime is not None:
            return runtime
        root_dir = ctx.session.artifact_dir / "browser"
        runtime = self.runtime_factory(root_dir, headless)
        self._runtimes[ctx.session.id] = runtime
        return runtime

    def _default_runtime_factory(self, root_dir: Path, headless: bool) -> BrowserRuntime:
        return BrowserRuntime.start(root_dir=root_dir, headless=headless)

    def _execute(self, code: str, namespace: Dict[str, Any]) -> Any:
        try:
            compiled = compile(code, "<llm-browser-python>", "eval")
        except SyntaxError:
            exec(compile(code, "<llm-browser-python>", "exec"), namespace, namespace)
            return None
        return eval(compiled, namespace, namespace)


def _env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.lower() in {"1", "true", "yes", "on"}


def _is_jsonable(value: Any) -> bool:
    try:
        json.dumps(value)
        return True
    except TypeError:
        return False
