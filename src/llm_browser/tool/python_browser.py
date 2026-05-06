from __future__ import annotations

import contextlib
import io
import json
import mimetypes
import os
import shutil
import sys
import threading
import time
import traceback
import types
from io import BytesIO
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Dict, List, Optional

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolImage, ToolResult

if TYPE_CHECKING:
    from llm_browser.browser import BrowserRuntime

RuntimeFactory = Callable[[Path, bool], "BrowserRuntime"]


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
        ) -> Dict[str, Any]:
            return runtime.cdp(method, params=params, session_id=session_id)

        def new_tab(url: str = "about:blank") -> Dict[str, Any]:
            return runtime.new_tab(url)

        def js(
            expression: str,
            await_promise: bool = True,
            repl_mode: bool = True,
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

        def screenshot(label: str = "screenshot", attach: bool = True, full_page: bool = False) -> ToolImage:
            image = runtime.screenshot(label=label, attach=attach, full_page=full_page)
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
                "cdp": cdp,
                "new_tab": new_tab,
                "navigate": runtime.navigate,
                "tabs": runtime.tabs,
                "attach_tab": runtime.attach_tab,
                "js": js,
                "wait_for_load": wait_for_load,
                "wait_until": wait_until,
                "wait_for_selector": wait_for_selector,
                "wait_for_text": wait_for_text,
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
            }
        )
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
