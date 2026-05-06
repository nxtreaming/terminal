from __future__ import annotations

import contextlib
import concurrent.futures
import html
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

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolImage, ToolResult

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
        try:
            with _GLOBAL_EXEC_LOCK:
                with self._execution_cwd(ctx.session.cwd), contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
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
                "time": time,
                "display": _display,
            }
            _install_optional_imports(namespace)
            self._namespaces[ctx.session.id] = namespace

        def cdp(
            method: str,
            params: Optional[Dict[str, Any]] = None,
            session_id: Optional[str] = None,
            timeout_s: Optional[float] = None,
            retry: bool = True,
        ) -> Dict[str, Any]:
            return runtime.cdp(method, params=params, session_id=session_id, timeout_s=timeout_s, retry=retry)

        def new_tab(url: str = "about:blank") -> Dict[str, Any]:
            return runtime.new_tab(url)

        def navigate(url: str, wait: bool = True, timeout_s: float = 20.0, timeout: Optional[float] = None) -> Dict[str, Any]:
            if timeout is not None:
                timeout_s = timeout
            return runtime.navigate(url, wait=wait, timeout_s=timeout_s)

        def js(
            expression: str,
            await_promise: bool = True,
            repl_mode: Optional[bool] = None,
            user_gesture: bool = False,
        ) -> Any:
            return runtime.js(
                expression,
                await_promise=await_promise,
                repl_mode=repl_mode,
                user_gesture=user_gesture,
            )

        def wait_for_load(timeout_s: float = 20.0, timeout: Optional[float] = None) -> None:
            if timeout is not None:
                timeout_s = timeout
            runtime.wait_for_load(timeout_s=timeout_s)

        def wait_until(expression: str, timeout_s: float = 20.0, timeout: Optional[float] = None, interval_s: float = 0.25) -> Any:
            if timeout is not None:
                timeout_s = timeout
            return runtime.wait_until(expression, timeout_s=timeout_s, interval_s=interval_s)

        def wait_for_selector(
            selector: str,
            timeout_s: float = 20.0,
            timeout: Optional[float] = None,
            visible: bool = False,
        ) -> Any:
            if timeout is not None:
                timeout_s = timeout
            return runtime.wait_for_selector(selector, timeout_s=timeout_s, visible=visible)

        def wait_for_text(text: str, timeout_s: float = 20.0, timeout: Optional[float] = None) -> Any:
            if timeout is not None:
                timeout_s = timeout
            return runtime.wait_for_text(text, timeout_s=timeout_s)

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
                time.sleep(0.25)

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
            api_key = _browser_use_api_key()
            if not api_key:
                return {
                    "filename": upload_name,
                    "path": str(artifact_path),
                    "downloadUrl": local_url,
                    "cloud": False,
                    "note": "BROWSER_USE_API_KEY is not set; returning local file URL.",
                }
            try:
                cloud = _upload_to_browser_use_cloud(artifact_path, filename=upload_name, content_type=mime, api_key=api_key)
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
            response = requests.get(url, headers=request_headers, timeout=timeout)
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
                response = requests.get(source, headers={"User-Agent": "Mozilla/5.0"}, timeout=30)
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
                    response = requests.get(url, headers=request_headers, timeout=timeout)
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

                curl_result = _fetch_text_with_curl_cffi(url, max_chars=max_chars, timeout=timeout, headers=request_headers)
                if curl_result is not None:
                    if direct_error:
                        curl_result["direct_error"] = direct_error
                    if curl_result.get("ok") and str(curl_result.get("text") or "").strip():
                        return curl_result
                    if not direct_error:
                        direct_error = f"curl_cffi HTTP {curl_result.get('status')}"

            return _fetch_text_with_jina_reader(
                url,
                max_chars=max_chars,
                timeout=timeout,
                headers=request_headers,
                direct_error=direct_error,
            )

        def search_web(query: str, max_results: int = 8, timeout: float = 20.0) -> Dict[str, Any]:
            try:
                import requests
            except Exception as exc:
                raise RuntimeError("requests is not installed") from exc

            urls = [
                ("bing", f"https://www.bing.com/search?q={quote_plus(query)}"),
                ("bing_reader", _jina_reader_url(f"https://www.bing.com/search?q={quote_plus(query)}")),
            ]
            results: List[Dict[str, str]] = []
            attempts: List[Dict[str, Any]] = []
            for source, search_url in urls:
                try:
                    response = requests.get(search_url, headers=_browser_headers(), timeout=timeout)
                    text = response.text
                    attempts.append({"source": source, "status": response.status_code, "url": response.url, "chars": len(text)})
                    if source == "bing":
                        results.extend(_parse_bing_results(text, limit=max_results - len(results)))
                    else:
                        results.extend(_parse_markdown_links(text, limit=max_results - len(results)))
                except Exception as exc:
                    attempts.append({"source": source, "url": search_url, "error": str(exc)})
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
                        delay = next_allowed_at - time.monotonic()
                        if delay > 0:
                            time.sleep(delay)
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
                        time.sleep(min(wait_s, 90.0))
                        next_allowed_at = time.monotonic()
                        attempt += 1
            else:
                with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count) as executor:
                    for index, result_item in executor.map(fetch_one, enumerate(url_list)):
                        results[index] = result_item
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

        namespace.update(
            {
                "browser": runtime,
                "artifact_dir": ctx.session.artifact_dir,
                "download_dir": runtime.root_dir / "downloads",
                "cwd": ctx.session.cwd,
                "workspace_dir": ctx.session.cwd,
                "output_dir": ctx.session.cwd / "outputs",
                "output_path": output_path,
                "cdp": cdp,
                "new_tab": new_tab,
                "navigate": navigate,
                "tabs": runtime.tabs,
                "attach_tab": runtime.attach_tab,
                "js": js,
                "wait_for_load": wait_for_load,
                "wait_until": wait_until,
                "wait_for_selector": wait_for_selector,
                "wait_for_text": wait_for_text,
                "deep_text": deep_text,
                "click_text": click_text,
                "dismiss_cookie_banners": dismiss_cookie_banners,
                "screenshot": screenshot,
                "page_info": runtime.page_info,
                "visible_text": runtime.visible_text,
                "links": runtime.links,
                "click_at": runtime.click_at,
                "type_text": runtime.type_text,
                "press": runtime.press,
                "scroll": runtime.scroll,
                "load_helper": load_helper,
                "save_helper": save_helper,
                "save_artifact": save_artifact,
                "upload_artifact": upload_artifact,
                "create_download_url": create_download_url,
                "artifact_download_url": create_download_url,
                "download_file": download_file,
                "read_pdf_text": read_pdf_text,
                "fetch_text": fetch_text,
                "fetch_many_text": fetch_many_text,
                "search_web": search_web,
                "extract_links": extract_links,
                "extract_markdown_link_blocks": extract_markdown_link_blocks,
                "read_sitemap": read_sitemap,
                "curl_requests": namespace.get("curl_requests"),
            }
        )
        _install_browser_helpers_module(namespace)
        return namespace

    def _runtime(self, ctx: ToolContext, headless: bool) -> "BrowserRuntime":
        runtime = self._runtimes.get(ctx.session.id)
        if runtime is not None:
            return runtime
        root_dir = ctx.session.artifact_dir / "browser"
        runtime = self.runtime_factory(root_dir, headless)
        self._runtimes[ctx.session.id] = runtime
        return runtime

    def _default_runtime_factory(self, root_dir: Path, headless: bool) -> "BrowserRuntime":
        from llm_browser.browser import BrowserRuntime

        return BrowserRuntime.start(root_dir=root_dir, headless=headless)

    def _execute(self, code: str, namespace: Dict[str, Any]) -> Any:
        if _looks_like_statements(code):
            exec(compile(code, "<llm-browser-python>", "exec"), namespace, namespace)
            return None
        try:
            compiled = compile(code, "<llm-browser-python>", "eval")
        except SyntaxError:
            exec(compile(code, "<llm-browser-python>", "exec"), namespace, namespace)
            return None
        return eval(compiled, namespace, namespace)

    @contextlib.contextmanager
    def _execution_cwd(self, cwd: Path):
        with self._exec_lock:
            previous = Path.cwd()
            cwd.mkdir(parents=True, exist_ok=True)
            os.chdir(cwd)
            try:
                yield
            finally:
                os.chdir(previous)


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


def _browser_headers() -> Dict[str, str]:
    return {
        "User-Agent": (
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
            "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36"
        ),
        "Accept-Language": "en-US,en;q=0.9",
    }


def _jina_reader_url(url: str) -> str:
    if url.startswith(("https://r.jina.ai/http://", "http://r.jina.ai/http://")):
        return url
    return "https://r.jina.ai/http://" + url


def _fetch_text_result(
    requested_url: str,
    final_url: str,
    status_code: int,
    text: str,
    source: str,
    max_chars: int,
) -> Dict[str, Any]:
    truncated = len(text) > max_chars
    return {
        "ok": 200 <= status_code < 400,
        "url": requested_url,
        "final_url": final_url,
        "status": status_code,
        "source": source,
        "text": text[:max_chars],
        "chars": len(text),
        "truncated": truncated,
    }


def _fetch_text_with_curl_cffi(
    url: str,
    *,
    max_chars: int,
    timeout: float,
    headers: Dict[str, str],
) -> Optional[Dict[str, Any]]:
    try:
        from curl_cffi import requests as curl_requests
    except Exception:
        return None

    last_error: Optional[str] = None
    for impersonate in ("chrome136", "chrome124", "chrome120"):
        try:
            response = curl_requests.get(url, headers=headers, timeout=timeout, impersonate=impersonate)
            result = _fetch_text_result(url, response.url, response.status_code, response.text, "curl_cffi", max_chars)
            result["impersonate"] = impersonate
            return result
        except Exception as exc:
            last_error = str(exc)
    return {
        "ok": False,
        "url": url,
        "source": "curl_cffi",
        "error": last_error or "curl_cffi request failed",
        "text": "",
        "truncated": False,
    }


def _fetch_text_with_jina_reader(
    url: str,
    *,
    max_chars: int,
    timeout: float,
    headers: Dict[str, str],
    direct_error: Optional[str] = None,
) -> Dict[str, Any]:
    try:
        import requests
    except Exception as exc:
        raise RuntimeError("requests is not installed") from exc

    reader_url = _jina_reader_url(url)
    last_error: Optional[str] = None
    for attempt in range(3):
        try:
            response = requests.get(reader_url, headers=headers, timeout=max(timeout, 30.0))
            retry_delay = _jina_retry_delay(response.text)
            if retry_delay is not None and attempt < 2:
                time.sleep(min(max(retry_delay, 1.0), 30.0))
                continue
            result = _fetch_text_result(url, response.url, response.status_code, response.text, "jina", max_chars)
            if retry_delay is not None:
                result["ok"] = False
                result["rate_limited"] = True
                result["retry_after_s"] = retry_delay
            if direct_error:
                result["direct_error"] = direct_error
            return result
        except Exception as exc:
            last_error = str(exc)
            if attempt < 2:
                time.sleep(1.0 + attempt)
    return {
        "ok": False,
        "url": url,
        "source": "jina",
        "reader_url": reader_url,
        "direct_error": direct_error,
        "error": last_error or "jina reader request failed",
        "text": "",
        "truncated": False,
    }


def _jina_retry_delay(text: str) -> Optional[float]:
    stripped = text.strip()
    if not stripped.startswith("{"):
        return None
    try:
        payload = json.loads(stripped)
    except json.JSONDecodeError:
        return None
    code = payload.get("code")
    status = payload.get("status")
    code_text = str(code or "")
    status_text = str(status or "")
    message = str(payload.get("message") or "").lower()
    rate_limited = code_text.startswith("429") or status_text.startswith("429") or (
        "retryAfter" in payload and "rate limit" in message
    )
    if not rate_limited:
        return None
    retry_after = payload.get("retryAfter")
    try:
        return float(retry_after)
    except (TypeError, ValueError):
        return 5.0


def _count_values(values: Any) -> Dict[str, int]:
    counts: Dict[str, int] = {}
    for value in values:
        if not value:
            value = "-"
        counts[str(value)] = counts.get(str(value), 0) + 1
    return counts


def _parse_bing_results(page: str, limit: int) -> List[Dict[str, str]]:
    results: List[Dict[str, str]] = []
    if limit <= 0:
        return results
    try:
        from bs4 import BeautifulSoup

        soup = BeautifulSoup(page, "html.parser")
        for item in soup.select("li.b_algo"):
            link = item.select_one("h2 a") or item.find("a")
            if not link:
                continue
            title = link.get_text(" ", strip=True)
            url = str(link.get("href") or "")
            snippet = item.get_text(" ", strip=True)
            results.append({"title": title, "url": url, "snippet": snippet[:600], "source": "bing"})
            if len(results) >= limit:
                return results
    except Exception:
        pass

    for match in re.finditer(r'<a[^>]+href="([^"]+)"[^>]*>(.*?)</a>', page, flags=re.I | re.S):
        url = html.unescape(match.group(1))
        title = re.sub(r"<[^>]+>", " ", match.group(2))
        title = html.unescape(re.sub(r"\s+", " ", title)).strip()
        if title and url.startswith(("http://", "https://")):
            results.append({"title": title, "url": url, "snippet": "", "source": "bing"})
            if len(results) >= limit:
                break
    return results


def _parse_markdown_links(markdown: str, limit: int) -> List[Dict[str, str]]:
    results: List[Dict[str, str]] = []
    if limit <= 0:
        return results
    seen = set()
    for match in re.finditer(r"\[([^\]]{1,220})\]\((https?://[^)\s]+)\)", markdown):
        title = re.sub(r"\s+", " ", match.group(1)).strip()
        url = match.group(2).strip()
        if not title or url in seen:
            continue
        seen.add(url)
        results.append({"title": title, "url": url, "snippet": "", "source": "bing_reader"})
        if len(results) >= limit:
            break
    return results


def _extract_links(text: str, pattern: Optional[str] = None, limit: int = 1000) -> List[str]:
    if limit <= 0:
        return []
    compiled = re.compile(pattern) if pattern else None
    seen: set[str] = set()
    links: List[str] = []
    link_re = re.compile(
        r"\[[^\]]{0,300}\]\((https?://[^)\s]+)\)"
        r"|<loc[^>]*>\s*(https?://[^<\s]+)"
        r"|(https?://[^\s<>\]\)\"']+)",
        flags=re.I,
    )
    for match in link_re.finditer(text):
        url = html.unescape(next(group for group in match.groups() if group)).strip()
        url = url.rstrip(".,;")
        if not url or url in seen:
            continue
        if compiled and not compiled.search(url):
            continue
        seen.add(url)
        links.append(url)
        if len(links) >= limit:
            return links
    return links


def _extract_markdown_link_blocks(
    text: str,
    *,
    url_pattern: Optional[str] = None,
    max_lines_after: int = 8,
    limit: int = 1000,
) -> List[Dict[str, Any]]:
    if limit <= 0:
        return []
    compiled = re.compile(url_pattern) if url_pattern else None
    lines = text.splitlines()
    link_re = re.compile(r"^\s*(?:[-*]\s*)?\[([^\]]{1,300})\]\((https?://[^)\s]+)\)\s*$")
    cards: List[Dict[str, Any]] = []
    for index, line in enumerate(lines):
        match = link_re.match(line.strip())
        if not match:
            continue
        title = re.sub(r"\s+", " ", html.unescape(match.group(1))).strip()
        url = html.unescape(match.group(2)).strip()
        if compiled and not compiled.search(url):
            continue

        following: List[str] = []
        for next_line in lines[index + 1 :]:
            stripped = next_line.strip()
            if not stripped:
                continue
            if link_re.match(stripped) and following:
                break
            following.append(stripped)
            if len(following) >= max_lines_after:
                break
        cards.append({"title": title, "url": url, "lines": following})
        if len(cards) >= limit:
            break
    return cards


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


def _browser_use_api_key() -> Optional[str]:
    return os.environ.get("BROWSER_USE_API_KEY") or os.environ.get("BU_API_KEY")


def _browser_use_api_base() -> str:
    return (
        os.environ.get("BROWSER_USE_API_BASE_URL")
        or os.environ.get("BROWSER_USE_API_BASE")
        or "https://api.browser-use.com/api/v3"
    ).rstrip("/")


def _upload_to_browser_use_cloud(path: Path, filename: str, content_type: str, api_key: str) -> Dict[str, Any]:
    try:
        import requests
    except Exception as exc:
        raise RuntimeError("requests is not installed") from exc

    base_url = _browser_use_api_base()
    headers = {"X-Browser-Use-API-Key": api_key}
    session_response = requests.post(f"{base_url}/sessions", headers=headers, json={"keep_alive": True}, timeout=30)
    session_response.raise_for_status()
    session_data = session_response.json()
    session_id = str(session_data.get("id") or session_data.get("sessionId") or session_data.get("session_id") or "")
    if not session_id:
        raise RuntimeError(f"Browser Use session response did not include an id: {session_data}")

    upload_payload = {"files": [{"name": filename, "contentType": content_type}]}
    upload_response = requests.post(
        f"{base_url}/sessions/{session_id}/files/upload",
        headers=headers,
        json=upload_payload,
        timeout=30,
    )
    if upload_response.status_code == 422:
        upload_payload = {"files": [{"name": filename, "content_type": content_type}]}
        upload_response = requests.post(
            f"{base_url}/sessions/{session_id}/files/upload",
            headers=headers,
            json=upload_payload,
            timeout=30,
        )
    upload_response.raise_for_status()
    upload_data = upload_response.json()
    files = upload_data.get("files") or []
    if not files:
        raise RuntimeError(f"Browser Use upload response did not include files: {upload_data}")
    uploaded = files[0]
    upload_url = uploaded.get("uploadUrl") or uploaded.get("upload_url")
    remote_path = uploaded.get("path") or uploaded.get("filePath") or uploaded.get("file_path") or filename
    if not upload_url:
        raise RuntimeError(f"Browser Use upload response did not include uploadUrl: {upload_data}")

    put_response = requests.put(upload_url, data=path.read_bytes(), headers={"Content-Type": content_type}, timeout=60)
    put_response.raise_for_status()

    list_response = requests.get(
        f"{base_url}/sessions/{session_id}/files",
        headers=headers,
        params={"includeUrls": "true", "prefix": remote_path, "limit": 10},
        timeout=30,
    )
    list_response.raise_for_status()
    list_data = list_response.json()
    for item in list_data.get("files", []):
        item_path = str(item.get("path") or "")
        if item_path == remote_path or item_path.endswith(f"/{filename}") or item_path.endswith(filename):
            download_url = item.get("url") or item.get("downloadUrl") or item.get("download_url")
            if download_url:
                return {
                    "browserUseSessionId": session_id,
                    "remotePath": item_path or remote_path,
                    "downloadUrl": download_url,
                }
    raise RuntimeError(f"Browser Use file list did not include a download URL for {remote_path}: {list_data}")


def _looks_like_statements(code: str) -> bool:
    stripped = code.strip()
    if "\n" in stripped:
        return True
    statement_prefixes = (
        "import ",
        "from ",
        "for ",
        "while ",
        "if ",
        "with ",
        "try:",
        "def ",
        "class ",
        "return ",
        "raise ",
        "assert ",
        "print(",
    )
    return stripped.startswith(statement_prefixes) or "=" in stripped and "==" not in stripped


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


def _install_browser_helpers_module(namespace: Dict[str, Any]) -> None:
    module = types.ModuleType("browser_helpers")
    export_names = [
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
        "deep_text",
        "click_text",
        "dismiss_cookie_banners",
        "screenshot",
        "page_info",
        "visible_text",
        "links",
        "click_at",
        "type_text",
        "press",
        "scroll",
        "load_helper",
        "save_helper",
        "save_artifact",
        "upload_artifact",
        "create_download_url",
        "artifact_download_url",
        "download_file",
        "read_pdf_text",
        "search_web",
        "extract_links",
        "extract_markdown_link_blocks",
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

    module.__all__ = [name for name in export_names if hasattr(module, name)]
    sys.modules["browser_helpers"] = module
    sys.modules["browser_use"] = module
    sys.modules["browser_tools"] = module
