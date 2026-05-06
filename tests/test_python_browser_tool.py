from __future__ import annotations

import tempfile
import unittest
import os
from pathlib import Path
from typing import Any, Dict
from unittest.mock import Mock, patch

from llm_browser.session.store import SessionStore
from llm_browser.tool.context import ToolContext
from llm_browser.tool.python_browser import PythonBrowserTool
from llm_browser.tool.result import ToolImage


class FakeRuntime:
    def __init__(self, root_dir: Path, headless: bool) -> None:
        self.root_dir = root_dir
        self.headless = headless
        self.tab_urls = []
        self.last_load_timeout = None
        self.last_await_promise = None

    def cdp(self, method: str, params=None, session_id=None) -> Dict[str, Any]:
        return {"method": method, "params": params or {}, "session_id": session_id}

    def new_tab(self, url: str = "about:blank") -> Dict[str, Any]:
        self.tab_urls.append(url)
        return {"url": url}

    def tabs(self):
        return [{"url": url, "type": "page"} for url in self.tab_urls]

    def navigate(self, url: str, wait: bool = True, timeout_s: float = 20.0) -> Dict[str, Any]:
        self.tab_urls.append(url)
        return {"url": url, "wait": wait}

    def attach_tab(self, target_id=None, index=None, url_contains=None) -> Dict[str, Any]:
        return {"target_id": target_id, "index": index, "url_contains": url_contains}

    def visible_text(self, max_chars: int = 8000) -> str:
        return "Example visible text"[:max_chars]

    def links(self, limit: int = 200):
        return [{"text": "Example", "href": "https://example.com"}][:limit]

    def js(
        self,
        expression: str,
        await_promise: bool = False,
        repl_mode: bool = True,
        user_gesture: bool = False,
    ) -> Any:
        self.last_await_promise = await_promise
        if expression == "document.title":
            return "Example Domain"
        return None

    def wait_for_load(self, timeout_s: float = 20.0) -> None:
        self.last_load_timeout = timeout_s
        return None

    def wait_until(self, expression: str, timeout_s: float = 20.0, interval_s: float = 0.25) -> Any:
        self.last_load_timeout = timeout_s
        return {"expression": expression, "timeout_s": timeout_s, "interval_s": interval_s}

    def wait_for_selector(self, selector: str, timeout_s: float = 20.0, visible: bool = False) -> Any:
        self.last_load_timeout = timeout_s
        return {"selector": selector, "timeout_s": timeout_s, "visible": visible}

    def wait_for_text(self, text: str, timeout_s: float = 20.0) -> Any:
        self.last_load_timeout = timeout_s
        return {"text": text, "timeout_s": timeout_s}

    def screenshot(self, label: str = "screenshot", attach: bool = True, full_page: bool = False) -> ToolImage:
        path = self.root_dir / "shot.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"png-bytes")
        return ToolImage(label=label, path=str(path), order=1, ts_ms=123, url="https://example.com", title="Example")

    def page_info(self) -> Dict[str, Any]:
        return {"url": "https://example.com", "title": "Example"}

    def click_at(self, x: float, y: float, button: str = "left", clicks: int = 1) -> None:
        return None

    def type_text(self, text: str) -> None:
        return None

    def press(self, key: str) -> None:
        return None

    def scroll(self, dx: float = 0, dy: float = 500, x: float = 500, y: float = 500) -> None:
        return None

    def close(self) -> None:
        return None


class PythonBrowserToolTest(unittest.TestCase):
    def test_executes_code_and_emits_attached_screenshot(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root, headless: FakeRuntime(root, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": "\n".join(
                        [
                            "new_tab('https://example.com')",
                            "screenshot('loaded', attach=True)",
                            "Path('artifact.txt').write_text('saved')",
                            "saved_path = save_artifact('artifact.txt')",
                            "result = {'title': js('document.title'), 'cwd': str(Path.cwd()), 'saved_path': saved_path}",
                        ]
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["title"], "Example Domain")
            self.assertEqual(result.data["result"]["cwd"], str(session.cwd))
            self.assertTrue(Path(result.data["result"]["saved_path"]).exists())
            self.assertEqual(result.images[0].label, "loaded")

            provider_content = result.to_provider_content()
            self.assertIsInstance(provider_content, list)
            self.assertEqual(provider_content[0]["type"], "input_text")
            self.assertEqual(provider_content[1]["type"], "input_image")

            image_events = [event for event in store.events.read(session.id) if event.type == "tool.image"]
            self.assertEqual(len(image_events), 1)
            self.assertEqual(image_events[0].payload["image"]["label"], "loaded")

    def test_relative_state_dir_is_not_affected_by_python_cwd(self) -> None:
        previous = Path.cwd()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "root"
            root.mkdir()
            work = Path(tmp) / "work"
            work.mkdir()
            os.chdir(root)
            try:
                store = SessionStore(Path("state"))
                session = store.create(cwd=work)
                ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
                tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

                tool(ctx, {"headless": True, "code": "screenshot('cwd_check', attach=True)"})

                self.assertTrue((root / "state" / "sessions" / session.id / "events.jsonl").exists())
                self.assertFalse((work / "state").exists())
            finally:
                os.chdir(previous)

    def test_statement_imports_and_display_shim_are_supported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": "from IPython.display import display\nvalue = {'ok': True}\ndisplay(value)\nresult = value",
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"], {"ok": True})
            self.assertIn('"ok": true', result.text)

    def test_wait_for_load_accepts_timeout_alias(self) -> None:
        runtime_holder = {}

        def factory(root_dir: Path, headless: bool) -> FakeRuntime:
            runtime = FakeRuntime(root_dir, headless)
            runtime_holder["runtime"] = runtime
            return runtime

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=factory)

            result = tool(ctx, {"headless": True, "code": "wait_for_load(timeout=7); result = 'ok'"})

            self.assertTrue(result.data["ok"])
            self.assertEqual(runtime_holder["runtime"].last_load_timeout, 7)

    def test_wait_helpers_are_exposed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "result = {"
                        "'until': wait_until('window.ready', timeout=3), "
                        "'selector': wait_for_selector('#accept', visible=True), "
                        "'text': wait_for_text('Accept')"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["until"]["timeout_s"], 3)
            self.assertEqual(result.data["result"]["selector"]["selector"], "#accept")
            self.assertTrue(result.data["result"]["selector"]["visible"])
            self.assertEqual(result.data["result"]["text"]["text"], "Accept")

    def test_pypdf_shims_pypdf2_import_and_exposes_pdf_helpers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "from PyPDF2 import PdfReader as CompatReader\n"
                        "result = {"
                        "'compat': CompatReader.__module__.startswith('pypdf'), "
                        "'download_helper': callable(download_file), "
                        "'pdf_helper': callable(read_pdf_text)"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["compat"])
            self.assertTrue(result.data["result"]["download_helper"])
            self.assertTrue(result.data["result"]["pdf_helper"])

    def test_save_artifact_writes_bytes_without_explicit_mode(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": "path = save_artifact('bill.pdf', b'%PDF-binary')\nresult = {'path': path}",
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(Path(result.data["result"]["path"]).read_bytes(), b"%PDF-binary")

    def test_create_download_url_falls_back_to_local_file_url_without_browser_use_key(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            (session.cwd / "bill.pdf").write_bytes(b"%PDF")
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(ctx, {"headless": True, "code": "result = upload_artifact('bill.pdf')"})

            self.assertTrue(result.data["ok"])
            self.assertFalse(result.data["result"]["cloud"])
            self.assertTrue(result.data["result"]["downloadUrl"].startswith("file://"))

    def test_upload_artifact_uses_browser_use_v3_when_key_is_set(self) -> None:
        class Response:
            def __init__(self, payload: Dict[str, Any], status_code: int = 200) -> None:
                self.payload = payload
                self.status_code = status_code

            def json(self) -> Dict[str, Any]:
                return self.payload

            def raise_for_status(self) -> None:
                if self.status_code >= 400:
                    raise RuntimeError(f"status {self.status_code}")

        posts = []

        def fake_post(url: str, **kwargs: Any) -> Response:
            posts.append((url, kwargs.get("json")))
            if url.endswith("/sessions"):
                return Response({"id": "11111111-1111-1111-1111-111111111111"})
            if url.endswith("/files/upload"):
                return Response(
                    {
                        "files": [
                            {
                                "name": "bill.pdf",
                                "uploadUrl": "https://upload.example/bill.pdf",
                                "path": "uploads/bill.pdf",
                            }
                        ]
                    }
                )
            raise AssertionError(url)

        put = Mock(return_value=Response({}))
        get = Mock(return_value=Response({"files": [{"path": "uploads/bill.pdf", "url": "https://download.example/bill.pdf"}]}))

        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"BROWSER_USE_API_KEY": "test-key"}, clear=True):
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            (session.cwd / "bill.pdf").write_bytes(b"%PDF")
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.post", side_effect=fake_post), patch("requests.put", put), patch("requests.get", get):
                result = tool(ctx, {"headless": True, "code": "result = upload_artifact('bill.pdf')"})

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["cloud"])
            self.assertEqual(result.data["result"]["downloadUrl"], "https://download.example/bill.pdf")
            self.assertEqual(posts[1][1], {"files": [{"name": "bill.pdf", "contentType": "application/pdf"}]})
            put.assert_called_once()
            get.assert_called_once()

    def test_requests_gets_browser_headers_by_default(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "import http.server, socketserver, threading, json\n"
                        "seen = {}\n"
                        "class Handler(http.server.BaseHTTPRequestHandler):\n"
                        "    def do_GET(self):\n"
                        "        seen['ua'] = self.headers.get('User-Agent')\n"
                        "        seen['lang'] = self.headers.get('Accept-Language')\n"
                        "        self.send_response(200); self.end_headers(); self.wfile.write(b'ok')\n"
                        "    def log_message(self, *args): pass\n"
                        "server = socketserver.TCPServer(('127.0.0.1', 0), Handler)\n"
                        "threading.Thread(target=server.handle_request, daemon=True).start()\n"
                        "requests.get(f'http://127.0.0.1:{server.server_address[1]}/')\n"
                        "server.server_close()\n"
                        "result = seen\n"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertIn("Mozilla/5.0", result.data["result"]["ua"])
            self.assertIn("en-US", result.data["result"]["lang"])

    def test_js_helper_awaits_promises_by_default(self) -> None:
        runtime_holder = {}

        def factory(root_dir: Path, headless: bool) -> FakeRuntime:
            runtime = FakeRuntime(root_dir, headless)
            runtime_holder["runtime"] = runtime
            return runtime

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=factory)

            result = tool(ctx, {"headless": True, "code": "js('Promise.resolve(1)'); result = 'ok'"})

            self.assertTrue(result.data["ok"])
            self.assertIs(runtime_holder["runtime"].last_await_promise, True)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
