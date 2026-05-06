from __future__ import annotations

import json
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
        self.last_js_expression = None
        self.last_cdp_timeout = None
        self.last_cdp_retry = None
        self.last_navigate_timeout = None

    def cdp(self, method: str, params=None, session_id=None, timeout_s=None, retry=True) -> Dict[str, Any]:
        self.last_cdp_timeout = timeout_s
        self.last_cdp_retry = retry
        return {"method": method, "params": params or {}, "session_id": session_id}

    def new_tab(self, url: str = "about:blank") -> Dict[str, Any]:
        self.tab_urls.append(url)
        return {"url": url}

    def tabs(self):
        return [{"url": url, "type": "page"} for url in self.tab_urls]

    def navigate(self, url: str, wait: bool = True, timeout_s: float = 20.0) -> Dict[str, Any]:
        self.tab_urls.append(url)
        self.last_navigate_timeout = timeout_s
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
        self.last_js_expression = expression
        if expression == "document.title":
            return "Example Domain"
        if "const maxChars" in expression and "shadowRoot" in expression:
            return "Light text\nShadow ticket text"
        if "const needle" in expression and "clickElement" in expression:
            return {"clicked": True, "text": "Accept all", "tag": "BUTTON"}
        if "OneTrust.AllowAll" in expression:
            return {"clicked": False}
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

    def screenshot(
        self,
        label: str = "screenshot",
        attach: bool = True,
        full_page: bool = False,
        timeout_s: float = 8.0,
    ) -> ToolImage:
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

    def test_output_path_maps_home_user_outputs_to_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp) / "work")
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "path = output_path('/home/user/outputs/result.csv')\n"
                        "Path(path).write_text('a,b\\n1,2\\n')\n"
                        "result = path"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            saved = Path(result.data["result"])
            self.assertEqual(saved, session.cwd / "outputs" / "result.csv")
            self.assertEqual(saved.read_text(), "a,b\n1,2\n")

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

    def test_cdp_and_navigate_expose_timeout_controls(self) -> None:
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

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "raw = cdp('Runtime.evaluate', {'expression': '1'}, timeout_s=2, retry=False)\n"
                        "nav = navigate('https://example.com', timeout=4)\n"
                        "result = {'raw': raw, 'nav': nav}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(runtime_holder["runtime"].last_cdp_timeout, 2)
            self.assertFalse(runtime_holder["runtime"].last_cdp_retry)
            self.assertEqual(runtime_holder["runtime"].last_navigate_timeout, 4)

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

    def test_deep_text_and_click_text_are_shadow_dom_aware_helpers(self) -> None:
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

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": "result = {'text': deep_text(), 'clicked': click_text('Accept all')}",
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertIn("Shadow ticket text", result.data["result"]["text"])
            self.assertTrue(result.data["result"]["clicked"]["clicked"])
            self.assertIn("shadowRoot", runtime_holder["runtime"].last_js_expression)

    def test_dismiss_cookie_banners_uses_click_text_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(ctx, {"headless": True, "code": "result = dismiss_cookie_banners(timeout_s=1)"})

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["clicked"])
            self.assertEqual(result.data["result"]["kind"], "cookie-banner")

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

    def test_fetch_text_falls_back_to_jina_reader(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        calls = []

        def fake_get(url: str, **kwargs: Any) -> Response:
            calls.append((url, kwargs))
            if url == "https://blocked.example/page":
                raise TimeoutError("blocked")
            return Response("Title: readable page\n\nMarkdown Content:\nhello world", url)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", side_effect=fake_get), patch(
                "llm_browser.tool.python_browser._fetch_text_with_curl_cffi",
                return_value=None,
            ):
                result = tool(ctx, {"headless": True, "code": "result = fetch_text('https://blocked.example/page', max_chars=12)"})

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["source"], "jina")
            self.assertIn("blocked", result.data["result"]["direct_error"])
            self.assertEqual(result.data["result"]["text"], "Title: reada")
            self.assertTrue(calls[1][0].startswith("https://r.jina.ai/http://https://blocked.example/page"))

    def test_fetch_text_tries_curl_cffi_before_jina_reader(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 403) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", return_value=Response("Access Denied", "https://blocked.example/page")), patch(
                "llm_browser.tool.python_browser._fetch_text_with_curl_cffi",
                return_value={
                    "ok": True,
                    "url": "https://blocked.example/page",
                    "final_url": "https://blocked.example/page",
                    "status": 200,
                    "source": "curl_cffi",
                    "text": "curl worked",
                    "chars": 11,
                    "truncated": False,
                    "impersonate": "chrome136",
                },
            ):
                result = tool(ctx, {"headless": True, "code": "result = fetch_text('https://blocked.example/page')"})

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["source"], "curl_cffi")
            self.assertEqual(result.data["result"]["text"], "curl worked")
            self.assertEqual(result.data["result"]["direct_error"], "HTTP 403")

    def test_read_sitemap_extracts_large_url_lists(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        sitemap = "\n".join(
            [
                "<urlset>",
                "<loc>https://example.com/keep/a</loc>",
                "[B](https://example.com/keep/b)",
                "https://example.com/skip/c",
                "</urlset>",
            ]
        )

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", return_value=Response(sitemap, "https://example.com/sitemap.xml")):
                result = tool(
                    ctx,
                    {
                        "headless": True,
                        "code": (
                            "from browser_helpers import read_sitemap\n"
                            "result = read_sitemap('https://example.com/sitemap.xml', include='/keep/')"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertEqual(
                result.data["result"]["links"],
                ["https://example.com/keep/a", "https://example.com/keep/b"],
            )
            self.assertEqual(result.data["result"]["count"], 2)

    def test_fetch_text_retries_jina_rate_limit_body(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        calls = []

        def fake_get(url: str, **kwargs: Any) -> Response:
            calls.append((url, kwargs))
            if len(calls) == 1:
                return Response('{"code":429,"status":42903,"retryAfter":1,"message":"RateLimitTriggeredError"}', url)
            return Response("Title: ok\n\nMarkdown Content:\nhello after retry", url)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", side_effect=fake_get), patch("time.sleep") as sleep:
                result = tool(
                    ctx,
                    {
                        "headless": True,
                        "code": (
                            "from browser_helpers import fetch_text_result\n"
                            "result = fetch_text_result('https://blocked.example/page', use_jina=True)"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["source"], "jina")
            self.assertIn("hello after retry", result.data["result"]["text"])
            self.assertEqual(len(calls), 2)
            sleep.assert_called_once()

    def test_fetch_many_text_can_save_bulk_results(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        def fake_get(url: str, **kwargs: Any) -> Response:
            return Response(f"page for {url}", url)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", side_effect=fake_get):
                result = tool(
                    ctx,
                    {
                        "headless": True,
                        "code": (
                            "from browser_helpers import fetch_many_text\n"
                            "result = fetch_many_text(['https://example.com/a', 'https://example.com/b'], "
                            "max_workers=2, save_to='pages.json')"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            summary = result.data["result"]
            self.assertEqual(summary["count"], 2)
            self.assertEqual(summary["ok"], 2)
            saved = Path(summary["path"])
            self.assertTrue(saved.exists())
            self.assertIn("https://example.com/a", saved.read_text(encoding="utf-8"))

    def test_fetch_many_text_can_rate_limit_and_retry_bulk_results(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        calls: list[str] = []

        def fake_get(url: str, **kwargs: Any) -> Response:
            calls.append(url)
            if len(calls) <= 3:
                return Response(
                    json.dumps(
                        {
                            "data": None,
                            "retryAfter": 0.1,
                            "code": 429,
                            "status": 42903,
                            "message": "Per IP rate limit exceeded",
                        }
                    ),
                    url,
                    429,
                )
            return Response(f"page for {url}", url)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", side_effect=fake_get), patch("time.sleep") as sleep:
                result = tool(
                    ctx,
                    {
                        "headless": True,
                        "code": (
                            "from browser_helpers import fetch_many_text\n"
                            "result = fetch_many_text(['https://example.com/a'], "
                            "use_jina=True, requests_per_minute=60000, rate_limit_retries=1, save_to='pages.json')"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            summary = result.data["result"]
            self.assertEqual(summary["count"], 1)
            self.assertEqual(summary["ok"], 1)
            self.assertGreaterEqual(len(calls), 4)
            self.assertTrue(sleep.called)
            saved = Path(summary["path"])
            self.assertIn("page for", saved.read_text(encoding="utf-8"))

    def test_extract_markdown_link_blocks_captures_directory_cards(self) -> None:
        markdown = (
            "*   [Morrilton](https://www.tractorsupply.com/tsc/store_Morrilton-AR-72110_2306)\n"
            "944 Hwy 287\n\n"
            "Morrilton, AR 72110\n\n"
            "[(501) 477-2220](tel:501-477-2220)\n\n"
            "    *   [PetVet clinic](https://www.tractorsupply.com/tsc/services/petvet)\n"
            "*   [Mountain Home](https://www.tractorsupply.com/tsc/store_MountainHome-AR-72653_2865)\n"
            "1025 Hwy 62 East Suite 1\n\n"
            "Mountain Home, AR 72653\n"
        )

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
                        "from browser_helpers import extract_markdown_link_blocks\n"
                        f"markdown = {markdown!r}\n"
                        "result = extract_markdown_link_blocks(markdown, url_pattern=r'/tsc/store_', max_lines_after=4)"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            cards = result.data["result"]
            self.assertEqual(len(cards), 2)
            self.assertEqual(cards[0]["title"], "Morrilton")
            self.assertEqual(cards[0]["lines"][:2], ["944 Hwy 287", "Morrilton, AR 72110"])

    def test_browser_helpers_module_exports_session_helpers(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", return_value=Response("hello from helper", "https://example.com")):
                result = tool(
                    ctx,
                    {
                        "headless": True,
                        "code": (
                            "from browser_helpers import *\n"
                            "result = {"
                            "'text': fetch_text('https://example.com')[:5], "
                            "'structured': fetch_text_result('https://example.com')['source'], "
                            "'many': fetch_many_text(['https://example.com'], save_to='bulk.json')['count'], "
                            "'blocks': extract_markdown_link_blocks('[A](https://example.com/a)')[0]['title'], "
                            "'title': js('document.title')"
                            "}"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["text"], "hello")
            self.assertEqual(result.data["result"]["structured"], "direct")
            self.assertEqual(result.data["result"]["many"], 1)
            self.assertEqual(result.data["result"]["blocks"], "A")
            self.assertEqual(result.data["result"]["title"], "Example Domain")

    def test_browser_use_module_alias_exports_helpers(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", return_value=Response("hello from alias", "https://example.com")):
                result = tool(
                    ctx,
                    {
                        "headless": True,
                        "code": (
                            "from browser_use import *\n"
                            "result = fetch_text('https://example.com')"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"], "hello from alias")

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
