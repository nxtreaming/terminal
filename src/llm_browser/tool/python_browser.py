from __future__ import annotations

import contextlib
import concurrent.futures
import io
import json
import mimetypes
import os
import re
import shutil
import sys
import threading
import time
import traceback
import types
from io import BytesIO
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Dict, List, Optional
from urllib.parse import quote_plus

from llm_browser.browser.helpers import ensure_agent_helpers_file
from llm_browser.events.event import now_ms
from llm_browser.session.cancel import SessionCancelled
from llm_browser.tool.browser_artifacts import browser_use_api_key, upload_to_browser_use_cloud
from llm_browser.tool.browser_exports import help_browser, install_browser_helpers_module
from llm_browser.tool.browser_state import (
    clear_cookies as _browser_clear_cookies,
    clear_storage as _browser_clear_storage,
    get_cookies as _browser_get_cookies,
    grant_permissions as _browser_grant_permissions,
    reset_permissions as _browser_reset_permissions,
    set_cookie as _browser_set_cookie,
    storage_state as _browser_storage_state,
    wait_for_download as _browser_wait_for_download,
)
from llm_browser.tool.context import ToolContext
from llm_browser.tool.python_exec import CancellableTimeModule, cancellable_sleep, cancellation_trace, execute_python, execution_cwd, is_jsonable
from llm_browser.tool.result import ToolImage, ToolResult
from llm_browser.tool.web_fetch import *  # noqa: F403 - helper substrate intentionally re-exported locally.

if TYPE_CHECKING:
    from llm_browser.browser import BrowserRuntime

RuntimeFactory = Callable[[Path, bool], "BrowserRuntime"]
_GLOBAL_EXEC_LOCK = threading.RLock()


class PythonBrowserTool:
    """Persistent Python execution environment with browser helpers."""

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
        previous_cancel_check = getattr(self._runtime(ctx, headless=headless), "cancel_check", None)

        def check_cancel() -> None:
            ctx.check_cancel()

        runtime = self._runtime(ctx, headless=headless)
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
            return ToolResult(text=stdout.getvalue(), data={"stderr": err, "ok": False}, images=images)
        finally:
            if hasattr(runtime, "set_cancel_check"):
                runtime.set_cancel_check(previous_cancel_check)

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
            check_cancel()
            if params is not None and not isinstance(params, dict):
                raise TypeError("cdp params must be a dict when provided")
            merged_params = dict(params or {})
            merged_params.update(kwargs)
            return runtime.cdp(method, params=merged_params, session_id=session_id, timeout_s=timeout_s, retry=retry)

        def new_tab(url: str = "about:blank") -> Dict[str, Any]:
            check_cancel()
            return runtime.new_tab(url)

        def navigate(url: str, wait: bool = True, timeout_s: float = 20.0, timeout: Optional[float] = None) -> Dict[str, Any]:
            if timeout is not None:
                timeout_s = timeout
            check_cancel()
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
            check_cancel()
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
            check_cancel()
            runtime.wait_for_load(timeout_s=timeout_s)

        def wait_until(expression: str, timeout_s: float = 20.0, timeout: Optional[float] = None, interval_s: float = 0.25) -> Any:
            if timeout is not None:
                timeout_s = timeout
            check_cancel()
            return runtime.wait_until(expression, timeout_s=timeout_s, interval_s=interval_s)

        def wait_for_selector(
            selector: str,
            timeout_s: float = 20.0,
            timeout: Optional[float] = None,
            visible: bool = False,
        ) -> Any:
            if timeout is not None:
                timeout_s = timeout
            check_cancel()
            return runtime.wait_for_selector(selector, timeout_s=timeout_s, visible=visible)

        def wait_for_element(
            selector: str,
            timeout: float = 10.0,
            visible: bool = False,
            timeout_s: Optional[float] = None,
        ) -> Any:
            return wait_for_selector(
                selector,
                timeout_s=timeout if timeout_s is None else timeout_s,
                visible=visible,
            )

        def wait_for_text(text: str, timeout_s: float = 20.0, timeout: Optional[float] = None) -> Any:
            if timeout is not None:
                timeout_s = timeout
            check_cancel()
            return runtime.wait_for_text(text, timeout_s=timeout_s)

        def wait_for_network_idle(timeout_s: float = 10.0, timeout: Optional[float] = None, idle_ms: int = 500) -> bool:
            if timeout is not None:
                timeout_s = timeout
            check_cancel()
            handler = getattr(runtime, "wait_for_network_idle", None)
            if handler is None:
                return False
            return bool(handler(timeout_s=timeout_s, idle_ms=idle_ms))

        def deep_text(max_chars: int = 12000) -> str:
            script = f"""
(() => {{
  const maxChars = {int(max_chars)};
  const roots = [];
  const seenRoots = new Set();
  const seenLines = new Set();
  const lines = [];

  function addRoot(root) {{
    if (!root || seenRoots.has(root)) return;
    seenRoots.add(root);
    roots.push(root);
    let elements = [];
    try {{ elements = Array.from(root.querySelectorAll("*")); }} catch (_) {{}}
    for (const element of elements) {{
      if (element.shadowRoot) addRoot(element.shadowRoot);
    }}
  }}

  function isHiddenElement(element) {{
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return false;
    const tag = element.tagName;
    if (["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "SVG"].includes(tag)) return true;
    const style = getComputedStyle(element);
    return style.display === "none" || style.visibility === "hidden";
  }}

  function addText(value) {{
    const normalized = String(value || "").replace(/\\s+/g, " ").trim();
    if (!normalized || seenLines.has(normalized)) return;
    seenLines.add(normalized);
    lines.push(normalized);
  }}

  function walk(node) {{
    if (!node) return;
    if (node.nodeType === Node.TEXT_NODE) {{
      addText(node.nodeValue);
      return;
    }}
    if (node.nodeType !== Node.ELEMENT_NODE && node.nodeType !== Node.DOCUMENT_FRAGMENT_NODE) return;
    if (node.nodeType === Node.ELEMENT_NODE && isHiddenElement(node)) return;
    if (node.shadowRoot) walk(node.shadowRoot);
    for (const child of Array.from(node.childNodes || [])) walk(child);
  }}

  addRoot(document.body || document.documentElement);
  for (const root of roots) walk(root);
  return lines.join("\\n").slice(0, maxChars);
}})()
"""
            return str(runtime.js(script, await_promise=True, repl_mode=False) or "")

        def click_text(
            text_or_pattern: str,
            timeout_s: float = 5.0,
            regex: bool = False,
            exact: bool = False,
            case_sensitive: bool = False,
        ) -> Dict[str, Any]:
            deadline = time.monotonic() + timeout_s
            last_result: Dict[str, Any] = {"clicked": False, "matches": []}
            while True:
                check_cancel()
                result = runtime.js(
                    _click_text_script(
                        text_or_pattern,
                        regex=regex,
                        exact=exact,
                        case_sensitive=case_sensitive,
                    ),
                    await_promise=True,
                    repl_mode=False,
                    user_gesture=True,
                )
                if isinstance(result, dict):
                    last_result = result
                    if result.get("clicked"):
                        return result
                if time.monotonic() >= deadline:
                    return last_result
                sleep(0.25)

        def dismiss_cookie_banners(timeout_s: float = 5.0, prefer: str = "accept") -> Dict[str, Any]:
            deadline = time.monotonic() + timeout_s
            vendor_result = runtime.js(_dismiss_cookie_vendor_script(prefer), await_promise=True, repl_mode=False, user_gesture=True)
            if isinstance(vendor_result, dict) and vendor_result.get("clicked"):
                return vendor_result

            accept_patterns = [
                r"^accept all$",
                r"^accept cookies?$",
                r"^allow all$",
                r"^agree$",
                r"^i agree$",
                r"^got it$",
                r"^ok$",
                r"^continue$",
                r"^save settings$",
            ]
            reject_patterns = [
                r"^reject all$",
                r"^decline$",
                r"^necessary only$",
                r"^essential only$",
            ]
            patterns = accept_patterns if prefer != "reject" else reject_patterns + accept_patterns
            last_result: Dict[str, Any] = {"clicked": False, "matches": []}
            for pattern in patterns:
                check_cancel()
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                result = click_text(pattern, timeout_s=min(remaining, 1.25), regex=True, case_sensitive=False)
                last_result = result
                if result.get("clicked"):
                    result["kind"] = "cookie-banner"
                    result["pattern"] = pattern
                    return result
            return last_result

        def output_path(path: str = "") -> str:
            workspace_output_dir = ctx.session.cwd / "outputs"
            requested = Path(path).expanduser()
            if not path:
                workspace_output_dir.mkdir(parents=True, exist_ok=True)
                return str(workspace_output_dir)
            if requested.is_absolute():
                try:
                    relative = requested.relative_to("/home/user/outputs")
                except ValueError:
                    requested.parent.mkdir(parents=True, exist_ok=True)
                    return str(requested)
                target = workspace_output_dir / relative
            else:
                target = ctx.session.cwd / requested
            target.parent.mkdir(parents=True, exist_ok=True)
            return str(target)

        def load_helper(path: str) -> None:
            helper_path = Path(path).expanduser()
            if not helper_path.is_absolute():
                helper_path = ctx.session.cwd / helper_path
            code = helper_path.read_text(encoding="utf-8")
            exec(compile(code, str(helper_path), "exec"), namespace, namespace)

        def save_helper(name: str, code: str) -> str:
            safe_name = "".join(ch if ch.isalnum() or ch in {"-", "_", "."} else "_" for ch in name)
            if not safe_name.endswith(".py"):
                safe_name += ".py"
            path = ctx.session.artifact_dir / "helpers" / safe_name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(code, encoding="utf-8")
            return str(path)

        def agent_helpers_path() -> str:
            return str(ensure_agent_helpers_file(ctx.session.cwd))

        def reload_agent_helpers(path: Optional[str] = None) -> Dict[str, Any]:
            helper_path = Path(path).expanduser() if path else ensure_agent_helpers_file(ctx.session.cwd)
            if not helper_path.is_absolute():
                helper_path = ctx.session.cwd / helper_path
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
                if name not in module.__dict__:
                    continue
                value = module.__dict__[name]
                if name.startswith("_"):
                    continue
                namespace[name] = value
                exported.append(name)
            namespace["_agent_helpers_path"] = str(helper_path)
            namespace["_agent_helpers_loaded_mtime"] = helper_path.stat().st_mtime
            return {"path": str(helper_path), "exports": sorted(exported)}

        def save_artifact(name: str, content: Any = None, mode: str = "text") -> str:
            source = Path(name).expanduser()
            if content is None and source.exists():
                if not source.is_absolute():
                    source = (ctx.session.cwd / source).resolve()
                safe_name = source.name
                path = ctx.session.artifact_dir / "python-artifacts" / safe_name
                path.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, path)
                return str(path)
            safe_name = "".join(ch if ch.isalnum() or ch in {"-", "_", "."} else "_" for ch in name)
            path = ctx.session.artifact_dir / "python-artifacts" / safe_name
            path.parent.mkdir(parents=True, exist_ok=True)
            if mode == "bytes" or isinstance(content, (bytes, bytearray, memoryview)):
                path.write_bytes(bytes(content))
            else:
                path.write_text(str(content), encoding="utf-8")
            return str(path)

        def upload_artifact(path: str, filename: Optional[str] = None, content_type: Optional[str] = None) -> Dict[str, Any]:
            source = Path(path).expanduser()
            if not source.is_absolute():
                source = ctx.session.cwd / source
            source = source.resolve()
            if not source.exists():
                raise FileNotFoundError(str(source))
            artifact_path = Path(save_artifact(str(source)))
            upload_name = _safe_artifact_name(filename or artifact_path.name)
            mime = content_type or mimetypes.guess_type(upload_name)[0] or "application/octet-stream"
            local_url = artifact_path.as_uri()
            api_key = browser_use_api_key()
            if not api_key:
                return {
                    "filename": upload_name,
                    "path": str(artifact_path),
                    "downloadUrl": local_url,
                    "cloud": False,
                    "note": "BROWSER_USE_API_KEY is not set; returning local file URL.",
                }
            try:
                cloud = upload_to_browser_use_cloud(artifact_path, filename=upload_name, content_type=mime, api_key=api_key)
            except Exception as exc:
                return {
                    "filename": upload_name,
                    "path": str(artifact_path),
                    "downloadUrl": local_url,
                    "cloud": False,
                    "error": str(exc),
                    "note": "Browser Use upload failed; returning local file URL.",
                }
            return {"filename": upload_name, "path": str(artifact_path), "downloadUrl": cloud["downloadUrl"], "cloud": True, **cloud}

        def create_download_url(path: str, filename: Optional[str] = None, content_type: Optional[str] = None) -> str:
            return str(upload_artifact(path, filename=filename, content_type=content_type)["downloadUrl"])

        def download_file(url: str, path: Optional[str] = None, timeout: float = 30.0, headers: Optional[Dict[str, str]] = None) -> str:
            try:
                import requests
            except Exception as exc:
                raise RuntimeError("requests is not installed") from exc

            target = Path(path or Path(url.split("?", 1)[0]).name or "download.bin").expanduser()
            if not target.is_absolute():
                target = ctx.session.cwd / target
            target.parent.mkdir(parents=True, exist_ok=True)
            request_headers = {"User-Agent": "Mozilla/5.0"}
            if headers:
                request_headers.update(headers)
            check_cancel()
            response = requests.get(url, headers=request_headers, timeout=timeout)
            check_cancel()
            response.raise_for_status()
            target.write_bytes(response.content)
            return str(target)

        def read_pdf_text(source: str, max_pages: Optional[int] = None) -> str:
            try:
                from pypdf import PdfReader
            except Exception as exc:
                raise RuntimeError("pypdf is not installed") from exc

            source_path = Path(source).expanduser()
            stream: Any
            close_stream = False
            if source.startswith(("http://", "https://")):
                try:
                    import requests
                except Exception as exc:
                    raise RuntimeError("requests is not installed") from exc
                check_cancel()
                response = requests.get(source, headers={"User-Agent": "Mozilla/5.0"}, timeout=30)
                check_cancel()
                response.raise_for_status()
                stream = BytesIO(response.content)
                close_stream = True
            else:
                if not source_path.is_absolute():
                    source_path = ctx.session.cwd / source_path
                stream = source_path
            try:
                reader = PdfReader(stream)
                pages = reader.pages[:max_pages] if max_pages is not None else reader.pages
                return "\n".join(page.extract_text() or "" for page in pages)
            finally:
                if close_stream:
                    stream.close()

        def fetch_text(
            url: str,
            max_chars: int = 20000,
            use_jina: Any = "auto",
            timeout: float = 20.0,
            headers: Optional[Dict[str, str]] = None,
        ) -> Dict[str, Any]:
            try:
                import requests
            except Exception as exc:
                raise RuntimeError("requests is not installed") from exc

            mode = str(use_jina).lower()
            force_jina = use_jina is True or mode in {"1", "true", "yes", "always", "jina", "reader"}
            disable_jina = use_jina is False or mode in {"0", "false", "no", "never", "direct"}
            request_headers = _browser_headers()
            if headers:
                request_headers.update(headers)

            direct_error: Optional[str] = None
            if not force_jina:
                try:
                    check_cancel()
                    response = requests.get(url, headers=request_headers, timeout=timeout)
                    check_cancel()
                    text = response.text
                    result = _fetch_text_result(url, response.url, response.status_code, text, "direct", max_chars)
                    if response.ok and text.strip():
                        return result
                    direct_error = f"HTTP {response.status_code}"
                    if disable_jina:
                        return result
                except Exception as exc:
                    direct_error = str(exc)
                    if disable_jina:
                        return {
                            "ok": False,
                            "url": url,
                            "source": "direct",
                            "error": direct_error,
                            "text": "",
                            "truncated": False,
                        }

                check_cancel()
                curl_result = _fetch_text_with_curl_cffi(url, max_chars=max_chars, timeout=timeout, headers=request_headers)
                check_cancel()
                if curl_result is not None:
                    if direct_error:
                        curl_result["direct_error"] = direct_error
                    if curl_result.get("ok") and str(curl_result.get("text") or "").strip():
                        return curl_result
                    if not direct_error:
                        direct_error = f"curl_cffi HTTP {curl_result.get('status')}"

            check_cancel()
            return _fetch_text_with_jina_reader(
                url,
                max_chars=max_chars,
                timeout=timeout,
                headers=request_headers,
                direct_error=direct_error,
            )

        def html_to_text(markup: str, max_chars: int = 30000, remove_chrome: bool = True) -> str:
            return _html_to_readable_text(str(markup or ""), max_chars=max_chars, remove_chrome=remove_chrome)

        def fetch_readable_text(
            url: str,
            max_chars: int = 30000,
            use_jina: Any = "auto",
            timeout: float = 20.0,
            headers: Optional[Dict[str, str]] = None,
        ) -> Dict[str, Any]:
            result = fetch_text(url, max_chars=max_chars * 4, use_jina=use_jina, timeout=timeout, headers=headers)
            raw_text = str(result.get("text") or "")
            content_type = str(result.get("content_type") or "").lower()
            looks_like_html = "<html" in raw_text[:1000].lower() or "<body" in raw_text[:2000].lower() or "text/html" in content_type
            readable = _html_to_readable_text(raw_text, max_chars=max_chars) if looks_like_html else re.sub(r"\s+", " ", raw_text).strip()[:max_chars]
            cleaned = dict(result)
            cleaned["text"] = readable
            cleaned["chars"] = len(readable)
            cleaned["raw_chars"] = len(raw_text)
            cleaned["readable"] = True
            return cleaned

        def search_web(
            query: str,
            max_results: int = 8,
            timeout: float = 20.0,
            save_raw: Any = "auto",
            include_specialized: Any = "auto",
        ) -> Dict[str, Any]:
            try:
                import requests
            except Exception as exc:
                raise RuntimeError("requests is not installed") from exc

            raw_mode = str(save_raw).lower()
            save_raw_always = save_raw is True or raw_mode in {"1", "true", "yes", "always", "all"}
            save_raw_auto = raw_mode in {"auto", "failed", "empty"}
            urls = [
                ("bing", f"https://www.bing.com/search?q={quote_plus(query)}"),
                ("duckduckgo_html", f"https://html.duckduckgo.com/html/?q={quote_plus(query)}"),
                ("duckduckgo_lite", f"https://lite.duckduckgo.com/lite/?q={quote_plus(query)}"),
                ("brave", f"https://search.brave.com/search?q={quote_plus(query)}"),
                ("google_reader", _jina_reader_url(f"https://www.google.com/search?q={quote_plus(query)}")),
                ("bing_reader", _jina_reader_url(f"https://www.bing.com/search?q={quote_plus(query)}")),
            ]
            results: List[Dict[str, str]] = []
            attempts: List[Dict[str, Any]] = []
            seen_urls: set[str] = set()

            def add_results(candidates: List[Dict[str, str]]) -> List[Dict[str, str]]:
                added: List[Dict[str, str]] = []
                for candidate in candidates:
                    url = _normalize_search_url(candidate.get("url", ""))
                    if not url or url in seen_urls:
                        continue
                    if not _looks_like_external_result_url(url):
                        continue
                    seen_urls.add(url)
                    item = dict(candidate)
                    item["url"] = url
                    results.append(item)
                    added.append(item)
                    if len(results) >= max_results:
                        break
                return added

            def save_search_page(source: str, text: str) -> str:
                search_dir = ctx.session.cwd / "search_pages"
                search_dir.mkdir(parents=True, exist_ok=True)
                slug = re.sub(r"[^a-zA-Z0-9_.-]+", "-", query).strip("-")[:80] or "query"
                path = search_dir / f"{int(time.time() * 1000)}-{source}-{slug}.html"
                path.write_text(text, encoding="utf-8", errors="replace")
                return str(path)

            cve_ids = _extract_cve_ids(query)
            if cve_ids:
                found, attempt = _search_cve_records(cve_ids, limit=max_results)
                added = add_results(found)
                attempt["parsed"] = len(added)
                attempts.append(attempt)

            fcc_codes = _extract_fcc_grantee_codes(query)
            if fcc_codes and len(results) < max_results:
                found, attempt = _search_fcc_grantee_records(fcc_codes, limit=max_results - len(results))
                added = add_results(found)
                attempt["parsed"] = len(added)
                attempts.append(attempt)

            for source, search_url in urls:
                if len(results) >= max_results:
                    break
                try:
                    check_cancel()
                    source_timeout = min(timeout, 12.0 if source.endswith("_reader") else 6.0)
                    response = requests.get(search_url, headers=_browser_headers(), timeout=source_timeout)
                    check_cancel()
                    text = response.text
                    if source == "bing":
                        parsed = _parse_bing_results(text, limit=max_results - len(results))
                    elif source.startswith("duckduckgo"):
                        parsed = _parse_duckduckgo_results(text, limit=max_results - len(results))
                    elif source == "brave":
                        parsed = _parse_brave_results(text, limit=max_results - len(results))
                    elif source.endswith("_reader"):
                        parsed = _parse_markdown_links(text, limit=max_results - len(results), source=source)
                    else:
                        parsed = _parse_generic_search_results(text, source=source, limit=max_results - len(results))
                    added = add_results(parsed)
                    attempt: Dict[str, Any] = {
                        "source": source,
                        "status": response.status_code,
                        "url": response.url,
                        "chars": len(text),
                        "parsed": len(added),
                    }
                    if save_raw_always or (save_raw_auto and not added):
                        attempt["raw_path"] = save_search_page(source, text)
                    attempts.append(attempt)
                except Exception as exc:
                    attempts.append({"source": source, "url": search_url, "error": str(exc)})
                if len(results) >= max_results:
                    break
            specialized_mode = str(include_specialized).lower()
            specialized_enabled = (
                include_specialized is True
                or specialized_mode in {"1", "true", "yes", "always"}
                or (specialized_mode == "auto" and _query_looks_scholarly(query))
            )
            if specialized_enabled and len(results) < max_results:
                for source, searcher in (
                    ("wikipedia_api", _search_wikipedia_api),
                    ("pubmed_api", _search_pubmed_api),
                    ("crossref_api", _search_crossref_api),
                ):
                    try:
                        found, attempt = searcher(query, limit=max_results - len(results), timeout=timeout)
                        check_cancel()
                        added = add_results(found)
                        attempt["parsed"] = len(added)
                        attempts.append(attempt)
                    except Exception as exc:
                        attempts.append({"source": source, "error": str(exc)})
                    if len(results) >= max_results:
                        break
            return {"query": query, "results": results[:max_results], "attempts": attempts}

        def extract_links(text: str, pattern: Optional[str] = None, limit: int = 1000) -> List[str]:
            links = _extract_links(str(text), pattern=pattern, limit=limit)
            return links

        def extract_markdown_link_blocks(
            text: str,
            url_pattern: Optional[str] = None,
            max_lines_after: int = 8,
            limit: int = 1000,
        ) -> List[Dict[str, Any]]:
            return _extract_markdown_link_blocks(
                str(text),
                url_pattern=url_pattern,
                max_lines_after=max_lines_after,
                limit=limit,
            )

        def extract_emails(
            text: str,
            domains: Optional[Any] = None,
            max_results: int = 200,
            include_context: bool = True,
        ) -> List[Dict[str, str]]:
            return _extract_email_records(
                str(text),
                domains=_normalize_email_domains(domains),
                max_results=max_results,
                include_context=include_context,
            )

        def crawl_site(
            start_url: str,
            max_pages: int = 12,
            timeout: float = 12.0,
            max_workers: int = 6,
            max_chars_per_page: int = 120000,
            use_jina: Any = "auto",
            same_site: bool = True,
            include: Optional[str] = None,
            exclude: Optional[str] = None,
            purpose: str = "contact",
        ) -> Dict[str, Any]:
            return _crawl_site(
                fetch_many_text=fetch_many_text,
                start_url=start_url,
                max_pages=max_pages,
                timeout=timeout,
                max_workers=max_workers,
                max_chars_per_page=max_chars_per_page,
                use_jina=use_jina,
                same_site=same_site,
                include=include,
                exclude=exclude,
                purpose=purpose,
            )

        def extract_store_locator_locations(
            target: str,
            provider: str = "auto",
            country_ids: Optional[Any] = None,
            max_locations: int = 10000,
            timeout: float = 30.0,
            save_to: Optional[str] = None,
            include_locations: bool = True,
        ) -> Dict[str, Any]:
            result = _extract_store_locator_locations(
                target,
                provider=provider,
                country_ids=country_ids,
                max_locations=max_locations,
                timeout=timeout,
            )
            locations = result.get("locations")
            if save_to and isinstance(locations, list):
                target_path = Path(save_to).expanduser()
                if not target_path.is_absolute():
                    target_path = ctx.session.cwd / target_path
                target_path.parent.mkdir(parents=True, exist_ok=True)
                target_path.write_text(json.dumps(locations, ensure_ascii=False, indent=2), encoding="utf-8")
                result["path"] = str(target_path)
            if not include_locations:
                result.pop("locations", None)
            return result

        store_locator_locations = extract_store_locator_locations

        def read_sitemap(
            url: str,
            include: Optional[str] = None,
            exclude: Optional[str] = None,
            max_urls: int = 10000,
            timeout: float = 30.0,
            use_jina: Any = "auto",
        ) -> Dict[str, Any]:
            result = fetch_text(url, max_chars=2_000_000, use_jina=use_jina, timeout=timeout)
            text = str(result.get("text") or "")
            links = _extract_links(text, pattern=include, limit=max(max_urls * 2, max_urls))
            if exclude:
                exclude_re = re.compile(exclude)
                links = [link for link in links if not exclude_re.search(link)]
            return {
                "url": url,
                "source": result.get("source"),
                "status": result.get("status"),
                "chars": result.get("chars"),
                "truncated": result.get("truncated"),
                "links": links[:max_urls],
                "count": len(links),
            }

        def fetch_many_text(
            urls: List[str],
            max_workers: int = 8,
            max_chars: int = 20000,
            use_jina: Any = "auto",
            timeout: float = 20.0,
            headers: Optional[Dict[str, str]] = None,
            save_to: Optional[str] = None,
            requests_per_minute: Optional[float] = None,
            rate_limit_retries: int = 3,
        ) -> Dict[str, Any]:
            url_list = [str(url) for url in urls]
            worker_count = max(1, min(int(max_workers), 64, len(url_list) or 1))
            results: List[Optional[Dict[str, Any]]] = [None] * len(url_list)

            def save_results() -> Optional[Path]:
                if not save_to:
                    return None
                target = Path(save_to).expanduser()
                if not target.is_absolute():
                    target = ctx.session.cwd / target
                target.parent.mkdir(parents=True, exist_ok=True)
                compact = [item or {"ok": False, "error": "missing result", "text": ""} for item in results]
                target.write_text(json.dumps(compact, ensure_ascii=False, indent=2), encoding="utf-8")
                return target

            def fetch_one(index_and_url: tuple[int, str]) -> tuple[int, Dict[str, Any]]:
                index, item_url = index_and_url
                try:
                    check_cancel()
                    return index, fetch_text(
                        item_url,
                        max_chars=max_chars,
                        use_jina=use_jina,
                        timeout=timeout,
                        headers=headers,
                    )
                except Exception as exc:
                    return index, {
                        "ok": False,
                        "url": item_url,
                        "source": "fetch_many_text",
                        "error": str(exc),
                        "text": "",
                        "truncated": False,
                    }

            rpm = None
            if requests_per_minute is not None:
                try:
                    rpm = float(requests_per_minute)
                except (TypeError, ValueError):
                    raise ValueError("requests_per_minute must be a positive number") from None
                if rpm <= 0:
                    raise ValueError("requests_per_minute must be a positive number")

            if rpm is not None:
                min_interval_s = 60.0 / rpm
                next_allowed_at = time.monotonic()
                retry_count = max(0, int(rate_limit_retries))
                for index, item_url in enumerate(url_list):
                    attempt = 0
                    while True:
                        check_cancel()
                        delay = next_allowed_at - time.monotonic()
                        if delay > 0:
                            sleep(delay)
                        _, result_item = fetch_one((index, item_url))
                        results[index] = result_item
                        save_results()
                        next_allowed_at = time.monotonic() + min_interval_s
                        if not result_item.get("rate_limited") or attempt >= retry_count:
                            break
                        retry_after = result_item.get("retry_after_s")
                        try:
                            wait_s = max(float(retry_after), min_interval_s)
                        except (TypeError, ValueError):
                            wait_s = max(5.0, min_interval_s)
                        sleep(min(wait_s, 90.0))
                        next_allowed_at = time.monotonic()
                        attempt += 1
            else:
                executor = concurrent.futures.ThreadPoolExecutor(max_workers=worker_count)
                pending: set[concurrent.futures.Future[tuple[int, Dict[str, Any]]]] = set()
                try:
                    pending = {executor.submit(fetch_one, item) for item in enumerate(url_list)}
                    while pending:
                        done, pending = concurrent.futures.wait(
                            pending,
                            timeout=0.05,
                            return_when=concurrent.futures.FIRST_COMPLETED,
                        )
                        check_cancel()
                        for future in done:
                            index, result_item = future.result()
                            results[index] = result_item
                except BaseException:
                    for future in pending:
                        future.cancel()
                    executor.shutdown(wait=False, cancel_futures=True)
                    raise
                else:
                    executor.shutdown(wait=True)
                save_results()

            compact_results = [item or {"ok": False, "error": "missing result", "text": ""} for item in results]
            summary: Dict[str, Any] = {
                "count": len(compact_results),
                "ok": sum(1 for item in compact_results if item.get("ok")),
                "failed": sum(1 for item in compact_results if not item.get("ok")),
                "truncated": sum(1 for item in compact_results if item.get("truncated")),
                "rate_limited": sum(1 for item in compact_results if item.get("rate_limited")),
                "sources": _count_values(str(item.get("source") or "") for item in compact_results),
            }
            if save_to:
                target = save_results()
                summary["path"] = str(target)
                return summary
            summary["results"] = compact_results
            return summary

        def screenshot(
            label: str = "screenshot",
            attach: bool = True,
            full_page: bool = False,
            timeout_s: float = 8.0,
        ) -> ToolImage:
            image = runtime.screenshot(label=label, attach=attach, full_page=full_page, timeout_s=timeout_s)
            if attach:
                images.append(image)
                ctx.emit_image(image)
            return image

        def capture_screenshot(
            path: Optional[str] = None,
            full: bool = False,
            max_dim: Optional[int] = None,
            attach: bool = True,
            label: Optional[str] = None,
            timeout_s: float = 8.0,
        ) -> str:
            target_path: Optional[Path] = Path(path).expanduser() if path else None
            if target_path is not None and not target_path.is_absolute():
                target_path = ctx.session.cwd / target_path
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
                attached = ToolImage(
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
                images.append(attached)
                ctx.emit_image(attached)
            return str(image_path)

        def screenshot_element(
            selector: str,
            label: Optional[str] = None,
            attach: bool = True,
            padding: float = 8.0,
            timeout_s: float = 8.0,
        ) -> Dict[str, Any]:
            selector_json = json.dumps(selector)
            rect = runtime.js(
                f"""
                (() => {{
                  const el = document.querySelector({selector_json});
                  if (!el) return null;
                  const r = el.getBoundingClientRect();
                  return {{
                    x: r.left + scrollX,
                    y: r.top + scrollY,
                    width: r.width,
                    height: r.height,
                    viewportX: r.left,
                    viewportY: r.top,
                    tag: el.tagName,
                    text: (el.innerText || el.alt || el.getAttribute('aria-label') || '').trim().slice(0, 300)
                  }};
                }})()
                """,
                await_promise=False,
            )
            if not isinstance(rect, dict):
                raise ValueError(f"element not found for selector: {selector}")
            pad = max(0.0, float(padding))
            clip = {
                "x": max(0.0, float(rect.get("x") or 0) - pad),
                "y": max(0.0, float(rect.get("y") or 0) - pad),
                "width": max(1.0, float(rect.get("width") or 1) + pad * 2),
                "height": max(1.0, float(rect.get("height") or 1) + pad * 2),
                "scale": 1.0,
            }
            image = runtime.screenshot(
                label=label or f"element_{selector}",
                attach=attach,
                full_page=False,
                timeout_s=timeout_s,
                clip=clip,
            )
            if attach:
                images.append(image)
                ctx.emit_image(image)
            return {"selector": selector, "rect": rect, "clip": clip, "image": image.to_dict()}

        def attach_image(path: str, label: Optional[str] = None, detail: str = "auto") -> ToolImage:
            image_path = Path(path).expanduser()
            if not image_path.is_absolute():
                image_path = ctx.session.cwd / image_path
            image_path = image_path.resolve()
            if not image_path.exists():
                raise FileNotFoundError(str(image_path))
            sidecar = image_path.with_suffix(".json")
            metadata: Dict[str, Any] = {}
            if sidecar.exists():
                try:
                    metadata = json.loads(sidecar.read_text(encoding="utf-8"))
                except Exception:
                    metadata = {}
            mime = mimetypes.guess_type(str(image_path))[0] or str(metadata.get("mime_type") or "image/png")
            image = ToolImage(
                label=label or str(metadata.get("label") or image_path.stem),
                path=str(image_path),
                mime_type=mime,
                detail=detail or str(metadata.get("detail") or "auto"),
                order=len(images) + 1,
                ts_ms=now_ms(),
                url=str(metadata.get("url") or ""),
                title=str(metadata.get("title") or ""),
                viewport=dict(metadata.get("viewport") or {}),
            )
            images.append(image)
            ctx.emit_image(image)
            return image

        def get_cookies(urls: Optional[List[str]] = None) -> Dict[str, Any]:
            return _browser_get_cookies(runtime, check_cancel, urls=urls)

        def set_cookie(cookie: Optional[Dict[str, Any]] = None, **kwargs: Any) -> Dict[str, Any]:
            return _browser_set_cookie(runtime, check_cancel, cookie=cookie, **kwargs)

        def clear_cookies() -> Dict[str, Any]:
            return _browser_clear_cookies(runtime, check_cancel)

        def storage_state(include_cookies: bool = True) -> Dict[str, Any]:
            return _browser_storage_state(runtime, check_cancel, include_cookies=include_cookies)

        def clear_storage(origin: Optional[str] = None, storage_types: str = "all") -> Dict[str, Any]:
            return _browser_clear_storage(runtime, check_cancel, origin=origin, storage_types=storage_types)

        def grant_permissions(permissions: List[str], origin: Optional[str] = None, browser_context_id: Optional[str] = None) -> Dict[str, Any]:
            return _browser_grant_permissions(runtime, check_cancel, permissions, origin=origin, browser_context_id=browser_context_id)

        def reset_permissions(browser_context_id: Optional[str] = None) -> Dict[str, Any]:
            return _browser_reset_permissions(runtime, check_cancel, browser_context_id=browser_context_id)

        def wait_for_download(pattern: Optional[str] = None, timeout_s: float = 30.0, poll_s: float = 0.25) -> Dict[str, Any]:
            return _browser_wait_for_download(runtime, check_cancel, pattern=pattern, timeout_s=timeout_s, poll_s=poll_s)

        def click_at_xy(x: float, y: float, button: str = "left", clicks: int = 1) -> None:
            check_cancel()
            runtime.click_at(x, y, button=button, clicks=clicks)

        def dispatch_key(selector: str, key: str = "Enter", event: str = "keypress") -> Any:
            key_code = _keyboard_code(key)
            selector_json = json.dumps(selector)
            key_json = json.dumps(key)
            event_json = json.dumps(event)
            return js(
                "(() => {"
                f"const e = document.querySelector({selector_json});"
                "if (!e) return false;"
                "e.focus();"
                f"e.dispatchEvent(new KeyboardEvent({event_json}, "
                f"{{key:{key_json}, code:{key_json}, keyCode:{key_code}, which:{key_code}, bubbles:true}}));"
                "return true;"
                "})()",
                await_promise=True,
            )

        def upload_file(selector: str, path: Any) -> Dict[str, Any]:
            files = path if isinstance(path, (list, tuple)) else [path]
            normalized_files = []
            for item in files:
                file_path = Path(str(item)).expanduser()
                if not file_path.is_absolute():
                    file_path = ctx.session.cwd / file_path
                normalized_files.append(str(file_path.resolve()))
            document = cdp("DOM.getDocument", depth=-1)
            root = document.get("root") if isinstance(document.get("root"), dict) else {}
            node_id = cdp("DOM.querySelector", nodeId=root.get("nodeId"), selector=selector).get("nodeId")
            if not node_id:
                raise RuntimeError(f"no element for {selector}")
            return cdp("DOM.setFileInputFiles", files=normalized_files, nodeId=node_id)

        def http_get(url: str, headers: Optional[Dict[str, str]] = None, timeout: float = 20.0) -> str:
            try:
                import requests
            except Exception as exc:
                raise RuntimeError("requests is not installed") from exc
            request_headers = {"User-Agent": "Mozilla/5.0"}
            if headers:
                request_headers.update(headers)
            check_cancel()
            response = requests.get(url, headers=request_headers, timeout=timeout)
            check_cancel()
            response.raise_for_status()
            return response.text

        downloads_dir = getattr(runtime, "downloads_dir", runtime.root_dir / "downloads")
        namespace.update(
            {
                "browser": runtime,
                "artifact_dir": ctx.session.artifact_dir,
                "download_dir": downloads_dir,
                "cwd": ctx.session.cwd,
                "workspace_dir": ctx.session.cwd,
                "output_dir": ctx.session.cwd / "outputs",
                "time": CancellableTimeModule(check_cancel),
                "sleep": sleep,
                "output_path": output_path,
                "cdp": cdp,
                "new_tab": new_tab,
                "navigate": navigate,
                "goto_url": goto_url,
                "tabs": runtime.tabs,
                "attach_tab": runtime.attach_tab,
                "js": js,
                "wait_for_load": wait_for_load,
                "wait_until": wait_until,
                "wait_for_selector": wait_for_selector,
                "wait_for_element": wait_for_element,
                "wait_for_text": wait_for_text,
                "wait_for_network_idle": wait_for_network_idle,
                "deep_text": deep_text,
                "click_text": click_text,
                "dismiss_cookie_banners": dismiss_cookie_banners,
                "screenshot": screenshot,
                "capture_screenshot": capture_screenshot,
                "screenshot_element": screenshot_element,
                "attach_image": attach_image,
                "page_info": runtime.page_info,
                "pending_dialog": getattr(runtime, "pending_dialog_info", lambda *args, **kwargs: None),
                "drain_cdp_events": getattr(runtime, "drain_events", lambda *args, **kwargs: []),
                "drain_events": getattr(runtime, "drain_events", lambda *args, **kwargs: []),
                "recent_cdp_events": getattr(runtime, "recent_cdp_events", lambda *args, **kwargs: []),
                "recent_console": getattr(runtime, "recent_console_events", lambda *args, **kwargs: []),
                "recent_network": getattr(runtime, "recent_network_events", lambda *args, **kwargs: []),
                "recent_network_failures": getattr(runtime, "recent_network_failures", lambda *args, **kwargs: []),
                "download_info": getattr(
                    runtime,
                    "download_info",
                    lambda *args, **kwargs: {"downloads_dir": str(downloads_dir), "files": [], "events": []},
                ),
                "wait_for_download": wait_for_download,
                "get_cookies": get_cookies,
                "set_cookie": set_cookie,
                "clear_cookies": clear_cookies,
                "storage_state": storage_state,
                "clear_storage": clear_storage,
                "grant_permissions": grant_permissions,
                "reset_permissions": reset_permissions,
                "save_browser_trace": getattr(
                    runtime,
                    "save_browser_trace",
                    lambda *args, **kwargs: {"path": None, "event_count": 0, "drained_count": 0},
                ),
                "visible_text": runtime.visible_text,
                "links": runtime.links,
                "click_at": runtime.click_at,
                "click_at_xy": click_at_xy,
                "fill_input": getattr(runtime, "fill_input", lambda *args, **kwargs: (_raise_runtime("fill_input is unavailable on this runtime"))),
                "type_text": runtime.type_text,
                "press": runtime.press,
                "press_key": getattr(runtime, "press_key", runtime.press),
                "dispatch_key": dispatch_key,
                "scroll": runtime.scroll,
                "list_tabs": getattr(runtime, "list_tabs", runtime.tabs),
                "current_tab": getattr(runtime, "current_tab", lambda: {}),
                "switch_tab": getattr(runtime, "switch_tab", runtime.attach_tab),
                "ensure_real_tab": getattr(runtime, "ensure_real_tab", lambda: None),
                "iframe_target": getattr(runtime, "iframe_target", lambda url_substr: None),
                "upload_file": upload_file,
                "load_helper": load_helper,
                "save_helper": save_helper,
                "agent_helpers_path": agent_helpers_path,
                "reload_agent_helpers": reload_agent_helpers,
                "help_browser": help_browser,
                "save_artifact": save_artifact,
                "upload_artifact": upload_artifact,
                "create_download_url": create_download_url,
                "artifact_download_url": create_download_url,
                "download_file": download_file,
                "read_pdf_text": read_pdf_text,
                "html_to_text": html_to_text,
                "http_get": http_get,
                "fetch_text": fetch_text,
                "fetch_readable_text": fetch_readable_text,
                "fetch_many_text": fetch_many_text,
                "search_web": search_web,
                "extract_links": extract_links,
                "extract_markdown_link_blocks": extract_markdown_link_blocks,
                "extract_emails": extract_emails,
                "crawl_site": crawl_site,
                "extract_store_locator_locations": extract_store_locator_locations,
                "store_locator_locations": store_locator_locations,
                "read_sitemap": read_sitemap,
                "curl_requests": namespace.get("curl_requests"),
                "check_cancel": check_cancel,
                "cancel_requested": cancel_requested,
            }
        )
        install_browser_helpers_module(namespace)
        _auto_reload_agent_helpers(ctx.session.cwd, namespace, reload_agent_helpers)
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

    def _default_runtime_factory(self, root_dir: Path, headless: bool) -> "BrowserRuntime":
        from llm_browser.browser import BrowserRuntime

        return BrowserRuntime.start(root_dir=root_dir, headless=headless)


def _env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.lower() in {"1", "true", "yes", "on"}


def _raise_runtime(message: str) -> None:
    raise RuntimeError(message)


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


def _keyboard_code(key: str) -> int:
    codes = {
        "Enter": 13,
        "Tab": 9,
        "Escape": 27,
        "Backspace": 8,
        " ": 32,
        "ArrowLeft": 37,
        "ArrowUp": 38,
        "ArrowRight": 39,
        "ArrowDown": 40,
        "Delete": 46,
        "Home": 36,
        "End": 35,
        "PageUp": 33,
        "PageDown": 34,
    }
    if key in codes:
        return codes[key]
    return ord(key) if len(key) == 1 else 0


def _auto_reload_agent_helpers(workspace: Path, namespace: Dict[str, Any], reload_agent_helpers: Callable[[], Dict[str, Any]]) -> None:
    helper_path = ensure_agent_helpers_file(workspace)
    try:
        mtime = helper_path.stat().st_mtime
    except OSError:
        return
    if namespace.get("_agent_helpers_loaded_mtime") == mtime:
        return
    reload_agent_helpers()


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


def _click_text_script(
    text_or_pattern: str,
    *,
    regex: bool,
    exact: bool,
    case_sensitive: bool,
) -> str:
    needle = json.dumps(text_or_pattern)
    return f"""
(() => {{
  const needle = {needle};
  const useRegex = {json.dumps(regex)};
  const exact = {json.dumps(exact)};
  const caseSensitive = {json.dumps(case_sensitive)};
  const roots = [];
  const seenRoots = new Set();

  function addRoot(root) {{
    if (!root || seenRoots.has(root)) return;
    seenRoots.add(root);
    roots.push(root);
    let elements = [];
    try {{ elements = Array.from(root.querySelectorAll("*")); }} catch (_) {{}}
    for (const element of elements) {{
      if (element.shadowRoot) addRoot(element.shadowRoot);
    }}
  }}

  function normalize(value) {{
    return String(value || "").replace(/\\s+/g, " ").trim();
  }}

  let compiled = null;
  if (useRegex) {{
    try {{
      compiled = new RegExp(needle, caseSensitive ? "" : "i");
    }} catch (error) {{
      return {{clicked: false, error: String(error), matches: []}};
    }}
  }}

  function isMatch(value) {{
    const text = normalize(value);
    if (!text) return false;
    if (compiled) return compiled.test(text);
    if (exact) return caseSensitive ? text === needle : text.toLowerCase() === needle.toLowerCase();
    return caseSensitive ? text.includes(needle) : text.toLowerCase().includes(needle.toLowerCase());
  }}

  function candidateText(element) {{
    const values = [
      element.innerText,
      element.textContent,
      element.value,
      element.getAttribute && element.getAttribute("aria-label"),
      element.getAttribute && element.getAttribute("title"),
      element.getAttribute && element.getAttribute("alt"),
      element.getAttribute && element.getAttribute("data-testid"),
      element.id,
    ];
    return normalize(values.filter(Boolean).join(" "));
  }}

  function isVisible(element) {{
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return false;
    const style = getComputedStyle(element);
    if (style.display === "none" || style.visibility === "hidden" || style.pointerEvents === "none") return false;
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }}

  function isClickable(element) {{
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return false;
    const tag = element.tagName;
    const role = (element.getAttribute("role") || "").toLowerCase();
    const className = String(element.className || "").toLowerCase();
    return (
      ["BUTTON", "A", "INPUT", "TEXTAREA", "SELECT", "SUMMARY", "LABEL"].includes(tag) ||
      role === "button" ||
      role === "link" ||
      element.hasAttribute("onclick") ||
      element.hasAttribute("tabindex") ||
      /\\b(btn|button|accept|agree|consent|cookie|save|reject|decline)\\b/.test(className)
    );
  }}

  function clickableTarget(element) {{
    let current = element;
    for (let i = 0; current && i < 8; i += 1) {{
      if (isClickable(current) && isVisible(current)) return current;
      current = current.parentElement || (current.getRootNode && current.getRootNode().host) || null;
    }}
    return isVisible(element) ? element : null;
  }}

  function describe(element, text) {{
    const rect = element.getBoundingClientRect();
    return {{
      tag: element.tagName,
      id: element.id || "",
      className: String(element.className || ""),
      role: element.getAttribute("role") || "",
      text: normalize(text).slice(0, 240),
      x: Math.round(rect.left + rect.width / 2),
      y: Math.round(rect.top + rect.height / 2),
    }};
  }}

  function clickElement(element) {{
    element.scrollIntoView({{block: "center", inline: "center"}});
    const rect = element.getBoundingClientRect();
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {{
      element.dispatchEvent(new MouseEvent(type, {{
        bubbles: true,
        cancelable: true,
        view: window,
        clientX: x,
        clientY: y,
        button: 0,
      }}));
    }}
    if (typeof element.click === "function") element.click();
  }}

  addRoot(document.body || document.documentElement);
  const matches = [];
  const seenElements = new Set();
  for (const root of roots) {{
    let elements = [];
    try {{
      elements = Array.from(root.querySelectorAll(
        "button,a,input,textarea,select,summary,label,[role='button'],[role='link'],[onclick],[tabindex],.btn,.button"
      ));
    }} catch (_) {{}}
    for (const element of elements) {{
      if (seenElements.has(element)) continue;
      seenElements.add(element);
      const text = candidateText(element);
      if (!isMatch(text)) continue;
      const target = clickableTarget(element);
      if (!target) continue;
      const item = describe(target, text);
      matches.push(item);
      clickElement(target);
      return {{clicked: true, ...item, matches: matches.slice(0, 10)}};
    }}
  }}
  return {{clicked: false, matches: matches.slice(0, 10)}};
}})()
"""


def _dismiss_cookie_vendor_script(prefer: str) -> str:
    prefer_reject = prefer == "reject"
    return f"""
(() => {{
  const preferReject = {json.dumps(prefer_reject)};
  const roots = [];
  const seenRoots = new Set();

  function addRoot(root) {{
    if (!root || seenRoots.has(root)) return;
    seenRoots.add(root);
    roots.push(root);
    let elements = [];
    try {{ elements = Array.from(root.querySelectorAll("*")); }} catch (_) {{}}
    for (const element of elements) {{
      if (element.shadowRoot) addRoot(element.shadowRoot);
    }}
  }}

  function visible(element) {{
    if (!element) return false;
    const style = getComputedStyle(element);
    const rect = element.getBoundingClientRect();
    return style.display !== "none" && style.visibility !== "hidden" && rect.width > 0 && rect.height > 0;
  }}

  function click(element, source) {{
    element.scrollIntoView({{block: "center", inline: "center"}});
    const rect = element.getBoundingClientRect();
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {{
      element.dispatchEvent(new MouseEvent(type, {{
        bubbles: true,
        cancelable: true,
        view: window,
        clientX: x,
        clientY: y,
        button: 0,
      }}));
    }}
    if (typeof element.click === "function") element.click();
    return {{
      clicked: true,
      source,
      tag: element.tagName,
      id: element.id || "",
      className: String(element.className || ""),
      text: String(element.innerText || element.textContent || element.value || "").replace(/\\s+/g, " ").trim().slice(0, 240),
    }};
  }}

  try {{
    if (!preferReject && window.OneTrust && typeof window.OneTrust.AllowAll === "function") {{
      window.OneTrust.AllowAll();
      return {{clicked: true, source: "OneTrust.AllowAll"}};
    }}
  }} catch (_) {{}}
  try {{
    if (window.Cookiebot && window.Cookiebot.dialog) {{
      if (preferReject && typeof window.Cookiebot.submitCustomConsent === "function") {{
        window.Cookiebot.submitCustomConsent(false, false, false);
        return {{clicked: true, source: "Cookiebot.submitCustomConsent"}};
      }}
      if (!preferReject && typeof window.Cookiebot.submitCustomConsent === "function") {{
        window.Cookiebot.submitCustomConsent(true, true, true);
        return {{clicked: true, source: "Cookiebot.submitCustomConsent"}};
      }}
    }}
  }} catch (_) {{}}

  const acceptSelectors = [
    "#onetrust-accept-btn-handler",
    "#accept-recommended-btn-handler",
    ".accept-recommended-btn-handler",
    "[data-testid='uc-accept-all-button']",
    "button[mode='primary']",
    "button[id*='accept' i]",
    "button[class*='accept' i]",
    "a[id*='accept' i]",
    "[role='button'][id*='accept' i]",
  ];
  const rejectSelectors = [
    "#onetrust-reject-all-handler",
    "#reject-recommended-btn-handler",
    "[data-testid='uc-deny-all-button']",
    "button[id*='reject' i]",
    "button[class*='reject' i]",
    "button[id*='decline' i]",
    "button[class*='decline' i]",
  ];
  const selectors = preferReject ? rejectSelectors.concat(acceptSelectors) : acceptSelectors.concat(rejectSelectors);
  addRoot(document.body || document.documentElement);
  for (const root of roots) {{
    for (const selector of selectors) {{
      let elements = [];
      try {{ elements = Array.from(root.querySelectorAll(selector)); }} catch (_) {{}}
      for (const element of elements) {{
        if (visible(element)) return click(element, selector);
      }}
    }}
  }}
  return {{clicked: false}};
}})()
"""


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


def _safe_artifact_name(name: str) -> str:
    safe_name = "".join(ch if ch.isalnum() or ch in {"-", "_", "."} else "_" for ch in Path(name).name)
    return safe_name or "artifact.bin"


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
