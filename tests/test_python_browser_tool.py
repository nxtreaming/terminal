from __future__ import annotations

import json
import tempfile
import threading
import time
import unittest
import os
from pathlib import Path
from typing import Any, Dict, Optional
from unittest.mock import Mock, patch

from llm_browser.session.cancel import SessionCancelled
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
        self.last_js_timeout_s = None
        self.last_screenshot_timeout = None
        self.last_network_idle_timeout = None
        self.last_network_idle_idle_ms = None
        self.cdp_calls = []

    def cdp(self, method: str, params=None, session_id=None, timeout_s=None, retry=True) -> Dict[str, Any]:
        self.last_cdp_timeout = timeout_s
        self.last_cdp_retry = retry
        self.cdp_calls.append({"method": method, "params": params or {}, "session_id": session_id})
        if method == "DOM.getDocument":
            return {"root": {"nodeId": 1}}
        if method == "DOM.querySelector":
            return {"nodeId": 7}
        if method == "DOM.setFileInputFiles":
            return {"ok": True, "files": list((params or {}).get("files") or [])}
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
        timeout_s: Optional[float] = None,
    ) -> Any:
        self.last_await_promise = await_promise
        self.last_js_expression = expression
        self.last_js_timeout_s = timeout_s
        if expression == "document.title":
            return "Example Domain"
        if "document.querySelector" in expression and "viewportX" in expression:
            return {"x": 100, "y": 200, "width": 320, "height": 180, "viewportX": 100, "viewportY": 200}
        if "const maxChars" in expression and "shadowRoot" in expression:
            return "Light text\nShadow ticket text"
        if "const needle" in expression and "clickElement" in expression:
            return {"clicked": True, "text": "Accept all", "tag": "BUTTON"}
        if "OneTrust.AllowAll" in expression:
            return {"clicked": False}
        if "KeyboardEvent" in expression:
            return True
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

    def wait_for_network_idle(self, timeout_s: float = 10.0, idle_ms: int = 500) -> bool:
        self.last_network_idle_timeout = timeout_s
        self.last_network_idle_idle_ms = idle_ms
        return True

    def screenshot(
        self,
        label: str = "screenshot",
        attach: bool = True,
        full_page: bool = False,
        timeout_s: float = 8.0,
        clip: Dict[str, float] | None = None,
    ) -> ToolImage:
        self.last_screenshot_timeout = timeout_s
        self.last_screenshot_clip = clip
        path = self.root_dir / "shot.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"png-bytes")
        return ToolImage(label=label, path=str(path), order=1, ts_ms=123, url="https://example.com", title="Example")

    def page_info(self) -> Dict[str, Any]:
        return {"url": "https://example.com", "title": "Example"}

    def download_info(self, limit: int = 100, drain: bool = True) -> Dict[str, Any]:
        return {
            "downloads_dir": str(self.root_dir / "downloads"),
            "files": [
                {
                    "name": "report.csv",
                    "relative_path": "report.csv",
                    "path": str(self.root_dir / "downloads" / "report.csv"),
                    "complete": True,
                }
            ],
            "events": [],
        }

    def click_at(self, x: float, y: float, button: str = "left", clicks: int = 1) -> None:
        return None

    def type_text(self, text: str) -> None:
        return None

    def press(self, key: str) -> None:
        return None

    def scroll(self, dx: float = 0, dy: float = 500, x: float = 500, y: float = 500) -> None:
        return None

    def drain_events(self, timeout_s: float = 0.05, max_events: int = 1000):
        return [{"method": "Page.loadEventFired"}]

    def close(self) -> None:
        return None


class CloudRuntime(FakeRuntime):
    def __init__(self, root_dir: Path, headless: bool) -> None:
        super().__init__(root_dir, headless)
        self.mode = "cloud"
        self.cloud_browser_id = "browser-1"
        self.cloud_live_url = "https://live.example/session"


class ReconnectedCloudRuntime(CloudRuntime):
    def __init__(self, root_dir: Path, headless: bool) -> None:
        super().__init__(root_dir, headless)
        self.reconnect_count = 0
        self.last_reconnect_reason = None

    def page_info(self) -> Dict[str, Any]:
        self.cloud_browser_id = "browser-2"
        self.cloud_live_url = "https://live.example/reconnected"
        self.reconnect_count = 1
        self.last_reconnect_reason = "started new Browser Use cloud browser"
        return {"url": "https://example.com", "title": "Example"}

    def connection_info(self) -> Dict[str, Any]:
        return {
            "mode": self.mode,
            "cloud_browser_id": self.cloud_browser_id,
            "cloud_live_url": self.cloud_live_url,
            "reconnect_count": self.reconnect_count,
            "last_reconnect_reason": self.last_reconnect_reason,
        }


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
                            "load_skill('artifacts')",
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

    def test_cloud_runtime_emits_live_preview_url_event(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root, headless: CloudRuntime(root, headless))

            result = tool(ctx, {"headless": True, "code": "result = 1"})
            second = tool(ctx, {"headless": True, "code": "result = 2"})

            self.assertTrue(result.data["ok"])
            self.assertTrue(second.data["ok"])
            live_events = [event for event in store.events.read(session.id) if event.type == "browser.live_url"]
            self.assertEqual(len(live_events), 1)
            self.assertEqual(live_events[0].payload["mode"], "cloud")
            self.assertEqual(live_events[0].payload["browser_id"], "browser-1")
            self.assertEqual(live_events[0].payload["live_url"], "https://live.example/session")

    def test_cloud_runtime_emits_new_live_preview_after_reconnect(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root, headless: ReconnectedCloudRuntime(root, headless))

            result = tool(ctx, {"headless": True, "code": "page_info(); result = 1"})

            self.assertTrue(result.data["ok"])
            live_events = [event for event in store.events.read(session.id) if event.type == "browser.live_url"]
            reconnect_events = [event for event in store.events.read(session.id) if event.type == "browser.reconnected"]
            self.assertEqual(len(live_events), 2)
            self.assertEqual(live_events[0].payload["live_url"], "https://live.example/session")
            self.assertEqual(live_events[1].payload["browser_id"], "browser-2")
            self.assertEqual(live_events[1].payload["live_url"], "https://live.example/reconnected")
            self.assertTrue(live_events[1].payload["reconnected"])
            self.assertEqual(len(reconnect_events), 1)
            self.assertEqual(reconnect_events[0].payload["reason"], "started new Browser Use cloud browser")
            self.assertEqual(reconnect_events[0].payload["reconnect_count"], 1)

    def test_screenshot_element_captures_clipped_element(self) -> None:
        runtime_holder: Dict[str, FakeRuntime] = {}

        def factory(root: Path, headless: bool) -> FakeRuntime:
            runtime = FakeRuntime(root, headless)
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
                    "code": "load_skill('dom_tools')\nresult = screenshot_element('#rate-table', label='rates', padding=10)",
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["selector"], "#rate-table")
            self.assertEqual(result.data["result"]["clip"]["x"], 90)
            self.assertEqual(result.data["result"]["clip"]["width"], 340)
            self.assertEqual(runtime_holder["runtime"].last_screenshot_clip["height"], 200)
            self.assertEqual(result.images[0].label, "rates")

    def test_image_helpers_accept_timeout_alias(self) -> None:
        runtime_holder: Dict[str, FakeRuntime] = {}

        def factory(root: Path, headless: bool) -> FakeRuntime:
            runtime = FakeRuntime(root, headless)
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
                    "code": "screenshot(timeout=6); capture_screenshot('alias.png', timeout=7); result = 'ok'",
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(runtime_holder["runtime"].last_screenshot_timeout, 7)

    def test_harnesless_aliases_are_available_in_persistent_python_tool(self) -> None:
        runtime_holder: Dict[str, FakeRuntime] = {}

        def factory(root: Path, headless: bool) -> FakeRuntime:
            runtime = FakeRuntime(root, headless)
            runtime_holder["runtime"] = runtime
            return runtime

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            upload_path = session.cwd / "upload.txt"
            upload_path.write_text("hello", encoding="utf-8")
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=factory)

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": "\n".join(
                        [
                            "goto_url('https://example.com')",
                            "load_skill('tracing')",
                            "load_skill('uploads')",
                            "load_skill('harnesless_compat')",
                            "nav = cdp('Page.navigate', url='https://example.com/raw')",
                            "shot = capture_screenshot('alias-shot.png', attach=True)",
                            "waited = wait_for_element('#app', timeout=2)",
                            "events = drain_events()",
                            "upload = upload_file('input[type=file]', 'upload.txt')",
                            "clicked = click_at_xy(10, 20)",
                            "keyed = dispatch_key('input[name=q]', 'Enter')",
                            "result = {'nav': nav, 'shot': shot, 'waited': waited, 'events': events, 'upload': upload, 'keyed': keyed}",
                        ]
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["nav"]["params"]["url"], "https://example.com/raw")
            self.assertTrue(Path(result.data["result"]["shot"]).exists())
            self.assertEqual(result.images[0].label, "alias-shot")
            self.assertEqual(result.data["result"]["waited"]["selector"], "#app")
            self.assertEqual(result.data["result"]["events"], [{"method": "Page.loadEventFired"}])
            self.assertEqual(result.data["result"]["upload"]["ok"], True)
            self.assertIn(str(upload_path), result.data["result"]["upload"]["files"])
            cdp_methods = [call["method"] for call in runtime_holder["runtime"].cdp_calls]
            self.assertIn("Page.navigate", cdp_methods)
            self.assertIn("DOM.setFileInputFiles", cdp_methods)

    def test_attach_image_rehydrates_existing_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            image_path = session.cwd / "old.png"
            image_path.write_bytes(b"png-bytes")
            image_path.with_suffix(".json").write_text(
                json.dumps(
                    {
                        "label": "old_frame",
                        "url": "https://example.com/old",
                        "title": "Old",
                        "viewport": {"width": 100, "height": 80},
                    }
                ),
                encoding="utf-8",
            )
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(ctx, {"headless": True, "code": "load_skill('artifacts')\nimage = attach_image('old.png'); result = image.to_dict()"})

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.images[0].label, "old_frame")
            self.assertEqual(result.data["result"]["url"], "https://example.com/old")
            self.assertEqual(result.images[0].viewport["width"], 100)
            image_events = [event for event in store.events.read(session.id) if event.type == "tool.image"]
            self.assertEqual(len(image_events), 1)

    def test_cancelled_python_code_propagates_session_cancelled(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))
            store.request_cancel(session.id, reason="test cancel")

            with self.assertRaises(SessionCancelled):
                tool(ctx, {"headless": True, "code": "for i in range(1000000):\n    pass"})

    def test_time_sleep_is_cooperatively_cancellable(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))
            caught: list[BaseException] = []

            def target() -> None:
                try:
                    tool(ctx, {"headless": True, "code": "time.sleep(5)\nresult = 'missed cancel'"})
                except BaseException as exc:
                    caught.append(exc)

            thread = threading.Thread(target=target)
            thread.start()
            time.sleep(0.15)
            store.request_cancel(session.id, reason="sleep cancel")
            thread.join(timeout=1.0)

            self.assertFalse(thread.is_alive())
            self.assertTrue(caught)
            self.assertIsInstance(caught[0], SessionCancelled)

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

    def test_agent_helpers_file_is_first_class_and_reloadable(self) -> None:
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
                        "path = Path(agent_helpers_path())\n"
                        "path.write_text('from browser_helpers import *\\n\\ndef helper_title():\\n    return js(\"document.title\")\\n')\n"
                        "loaded = reload_agent_helpers()\n"
                        "result = {'path': str(path), 'title': helper_title(), 'loaded': 'helper_title' in loaded['exports']}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertTrue(Path(result.data["result"]["path"]).exists())
            self.assertEqual(result.data["result"]["title"], "Example Domain")
            self.assertTrue(result.data["result"]["loaded"])

    def test_help_browser_is_available_in_namespace_and_helper_module(self) -> None:
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
                        "from browser_helpers import help_browser as imported_help\n"
                        "result = {'direct': 'fill_input' in help_browser(), 'imported': 'wait_for_network_idle' in imported_help()}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["direct"])
            self.assertTrue(result.data["result"]["imported"])

    def test_default_autoload_keeps_small_browser_surface(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
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
                        "'has_cdp': callable(cdp), "
                        "'has_http_get': callable(http_get), "
                        "'has_search': 'search_web' in globals(), "
                        "'module_has_search': hasattr(__import__('browser_helpers'), 'search_web'), "
                        "'has_load_skill': callable(load_skill), "
                        "'skill_names': [item['name'] for item in list_skills()], "
                        "'help_mentions_skills': 'load_skill' in help_browser()"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["has_cdp"])
            self.assertTrue(result.data["result"]["has_http_get"])
            self.assertFalse(result.data["result"]["has_search"])
            self.assertFalse(result.data["result"]["module_has_search"])
            self.assertTrue(result.data["result"]["has_load_skill"])
            self.assertIn("research", result.data["result"]["skill_names"])
            self.assertTrue(result.data["result"]["help_mentions_skills"])

    def test_browser_helpers_star_export_prefers_primary_surface(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "import browser_helpers\n"
                        "result = {"
                        "'star': browser_helpers.__all__, "
                        "'module_has_compat': all(hasattr(browser_helpers, name) for name in ['navigate', 'screenshot', 'tabs']), "
                        "'namespace_has_compat': all(name in globals() for name in ['navigate', 'screenshot', 'tabs'])"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            star = result.data["result"]["star"]
            self.assertIn("goto_url", star)
            self.assertIn("capture_screenshot", star)
            self.assertIn("click_at_xy", star)
            self.assertNotIn("navigate", star)
            self.assertNotIn("screenshot", star)
            self.assertNotIn("tabs", star)
            self.assertNotIn("wait_for_text", star)
            self.assertTrue(result.data["result"]["module_has_compat"])
            self.assertTrue(result.data["result"]["namespace_has_compat"])

    def test_legacy_autoload_can_restore_old_large_surface(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LLM_BROWSER_AUTOLOAD_SKILLS": "all"}, clear=True):
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "import browser_helpers\n"
                        "result = {"
                        "'has_search': callable(search_web), "
                        "'module_has_search': callable(browser_helpers.search_web), "
                        "'loaded': loaded_skills()"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["has_search"])
            self.assertTrue(result.data["result"]["module_has_search"])
            self.assertIn("search", result.data["result"]["loaded"])

    def test_load_skill_inject_false_returns_namespace_without_globals(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LLM_BROWSER_AUTOLOAD_SKILLS": "core"}, clear=False):
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", return_value=Response("hello skill", "https://example.com")):
                result = tool(
                    ctx,
                    {
                        "headless": True,
                        "code": (
                            "research = load_skill('research', inject=False)\n"
                            "payload = research.fetch_text('https://example.com')\n"
                            "result = {"
                            "'text': payload['text'], "
                            "'global_after': 'fetch_text' in globals(), "
                            "'loaded': loaded_skills()"
                            "}"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["text"], "hello skill")
            self.assertFalse(result.data["result"]["global_after"])
            self.assertEqual(result.data["result"]["loaded"], [])

    def test_load_skill_injects_exports_idempotently_and_reads_interaction_skill(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LLM_BROWSER_AUTOLOAD_SKILLS": "core"}, clear=False):
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "first = load_skill('dom_tools')\n"
                        "second = load_skill('dom_tools')\n"
                        "from browser_helpers import deep_text as imported_deep_text\n"
                        "result = {"
                        "'has_deep_text': callable(deep_text), "
                        "'module_updated': callable(imported_deep_text), "
                        "'first_exports': first['exports'], "
                        "'second_already': second['already_loaded'], "
                        "'loaded': loaded_skills(), "
                        "'iframe_skill': 'Iframes' in read_skill('iframes')"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["has_deep_text"])
            self.assertTrue(result.data["result"]["module_updated"])
            self.assertIn("deep_text", result.data["result"]["first_exports"])
            self.assertTrue(result.data["result"]["second_already"])
            self.assertEqual(result.data["result"]["loaded"], ["dom_tools"])
            self.assertTrue(result.data["result"]["iframe_skill"])

    def test_split_skills_make_specialized_helpers_explicit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LLM_BROWSER_AUTOLOAD_SKILLS": "core"}, clear=False):
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "load_skill('research')\n"
                        "after_research = {'fetch': callable(fetch_text), 'search': 'search_web' in globals()}\n"
                        "load_skill('search')\n"
                        "load_skill('cookie_banners')\n"
                        "load_skill('uploads')\n"
                        "load_skill('cloud_artifacts')\n"
                        "result = {"
                        "'after_research': after_research, "
                        "'has_search': callable(search_web), "
                        "'has_banner': callable(dismiss_cookie_banners), "
                        "'has_upload': callable(upload_file), "
                        "'has_cloud_upload': callable(upload_artifact), "
                        "'docs': 'Store Locators' in read_skill('store-locators'), "
                        "'loaded': loaded_skills()"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["after_research"]["fetch"])
            self.assertFalse(result.data["result"]["after_research"]["search"])
            self.assertTrue(result.data["result"]["has_search"])
            self.assertTrue(result.data["result"]["has_banner"])
            self.assertTrue(result.data["result"]["has_upload"])
            self.assertTrue(result.data["result"]["has_cloud_upload"])
            self.assertTrue(result.data["result"]["docs"])
            self.assertEqual(
                result.data["result"]["loaded"],
                ["cloud_artifacts", "cookie_banners", "research", "search", "uploads"],
            )

    def test_public_record_skill_can_be_loaded_with_dash_name(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LLM_BROWSER_AUTOLOAD_SKILLS": "core"}, clear=False):
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": (
                        "load_skill('public-records')\n"
                        "payload = search_cve_records('CVE-2020-8166 Rails details')\n"
                        "result = {'url': payload['results'][0]['url'], 'loaded': loaded_skills()}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["url"], "https://nvd.nist.gov/vuln/detail/CVE-2020-8166")
            self.assertEqual(result.data["result"]["loaded"], ["public_records"])

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

    def test_raw_cdp_accepts_timeout_alias_without_leaking_to_params(self) -> None:
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
                    "code": "result = cdp('Runtime.evaluate', expression='1', timeout=7)",
                },
            )

            self.assertTrue(result.data["ok"])
            runtime = runtime_holder["runtime"]
            self.assertEqual(runtime.last_cdp_timeout, 7)
            self.assertNotIn("timeout", runtime.cdp_calls[-1]["params"])

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
                        "load_skill('harnesless_compat')\n"
                        "result = {"
                        "'until': wait_until('window.ready', timeout=3), "
                        "'selector': wait_for_selector('#accept', visible=True), "
                        "'text': wait_for_text('Accept'), "
                        "'network': wait_for_network_idle(timeout=4, idle_ms=250)"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["until"]["timeout_s"], 3)
            self.assertEqual(result.data["result"]["selector"]["selector"], "#accept")
            self.assertTrue(result.data["result"]["selector"]["visible"])
            self.assertEqual(result.data["result"]["text"]["text"], "Accept")
            self.assertTrue(result.data["result"]["network"])
            self.assertEqual(tool._runtimes[session.id].last_network_idle_timeout, 4)
            self.assertEqual(tool._runtimes[session.id].last_network_idle_idle_ms, 250)

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
                    "code": "load_skill('dom_tools')\nresult = {'text': deep_text(), 'clicked': click_text('Accept all')}",
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

            result = tool(ctx, {"headless": True, "code": "load_skill('cookie_banners')\nresult = dismiss_cookie_banners(timeout_s=1)"})

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["clicked"])
            self.assertEqual(result.data["result"]["kind"], "cookie-banner")

    def test_cookie_storage_permission_and_download_helpers(self) -> None:
        runtime_holder: Dict[str, FakeRuntime] = {}

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
                        "load_skill('cookies')\n"
                        "load_skill('downloads')\n"
                        "result = {"
                        "'cookies': get_cookies(), "
                        "'set_cookie': set_cookie(name='token', value='abc'), "
                        "'clear_storage': clear_storage(), "
                        "'grant': grant_permissions(['clipboardReadWrite'], origin='https://example.com'), "
                        "'reset': reset_permissions(), "
                        "'download': wait_for_download('*.csv')"
                        "}"
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            methods = [call["method"] for call in runtime_holder["runtime"].cdp_calls]
            self.assertIn("Network.getCookies", methods)
            self.assertIn("Network.setCookie", methods)
            self.assertIn("Storage.clearDataForOrigin", methods)
            self.assertIn("Browser.grantPermissions", methods)
            self.assertIn("Browser.resetPermissions", methods)
            self.assertEqual(result.data["result"]["download"]["name"], "report.csv")

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
                        "load_skill('artifacts')\n"
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
                    "code": "load_skill('artifacts')\npath = save_artifact('bill.pdf', b'%PDF-binary')\nresult = {'path': path}",
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

            result = tool(ctx, {"headless": True, "code": "load_skill('cloud_artifacts')\nresult = upload_artifact('bill.pdf')"})

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
                result = tool(ctx, {"headless": True, "code": "load_skill('cloud_artifacts')\nresult = upload_artifact('bill.pdf')"})

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
                result = tool(ctx, {"headless": True, "code": "load_skill('research')\nresult = fetch_text('https://blocked.example/page', max_chars=12)"})

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
                result = tool(ctx, {"headless": True, "code": "load_skill('research')\nresult = fetch_text('https://blocked.example/page')"})

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["source"], "curl_cffi")
            self.assertEqual(result.data["result"]["text"], "curl worked")
            self.assertEqual(result.data["result"]["direct_error"], "HTTP 403")

    def test_fetch_readable_text_cleans_html_chrome(self) -> None:
        class Response:
            def __init__(self, text: str, url: str) -> None:
                self.text = text
                self.url = url
                self.status_code = 200
                self.ok = True
                self.headers = {"content-type": "text/html; charset=utf-8"}

        html_page = (
            "<html><head><title>Example</title><style>.x{}</style></head>"
            "<body><nav>Menu Home</nav><main><h1>Report Title</h1><p>Useful paragraph.</p>"
            "<script>bad()</script><p>Useful paragraph.</p></main><footer>Footer</footer></body></html>"
        )

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", return_value=Response(html_page, "https://example.com/report")):
                result = tool(ctx, {"headless": True, "code": "load_skill('research')\nresult = fetch_readable_text('https://example.com/report')"})

            self.assertTrue(result.data["ok"])
            text = result.data["result"]["text"]
            self.assertIn("Report Title", text)
            self.assertIn("Useful paragraph.", text)
            self.assertNotIn("Menu Home", text)
            self.assertNotIn("bad()", text)
            self.assertEqual(text.count("Useful paragraph."), 1)

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
                            "load_skill('extraction')\n"
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
                            "load_skill('research')\n"
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
                            "load_skill('research')\n"
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
                            "load_skill('research')\n"
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
                        "load_skill('extraction')\n"
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

    def test_extract_emails_filters_template_noise(self) -> None:
        text = (
            "Contact Founder@RealExample.com or hello@realexample.com. "
            "Ignore user@domain.com, you@company.com, error-lite@duckduckgo.com, "
            "example@mysite.com, and icon-zest-for-life@4x.png."
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
                    "code": f"load_skill('extraction')\nresult = extract_emails({text!r}, domains='realexample.com')",
                },
            )

            self.assertTrue(result.data["ok"])
            emails = [item["email"] for item in result.data["result"]]
            self.assertEqual(emails, ["founder@realexample.com", "hello@realexample.com"])
            self.assertIn("Contact Founder", result.data["result"][0]["context"])

    def test_crawl_site_fetches_contact_pages_and_extracts_email(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.content = text.encode("utf-8")
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400
                self.headers = {"content-type": "text/html"}

            def raise_for_status(self) -> None:
                if not self.ok:
                    raise RuntimeError(f"HTTP {self.status_code}")

        pages = {
            "https://example.com/": (
                "<html><title>Home</title><body>"
                "<a href='/contact'>Contact</a><a href='/team'>Team</a>"
                "</body></html>"
            ),
            "https://example.com/contact": (
                "<html><title>Contact</title><body>"
                "<a href='mailto:founder@realexample.com'>founder@realexample.com</a>"
                "</body></html>"
            ),
            "https://example.com/team": "<html><title>Team</title><body>CEO</body></html>",
        }

        def fake_get(url: str, **kwargs: Any) -> Response:
            normalized = url.rstrip("/") + ("/" if url.rstrip("/") == "https://example.com" else "")
            if normalized in pages:
                return Response(pages[normalized], normalized)
            return Response("", url, status_code=404)

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
                        "code": "load_skill('research')\nresult = crawl_site('https://example.com', max_pages=4, use_jina=False, timeout=5)",
                    },
                )

            self.assertTrue(result.data["ok"])
            payload = result.data["result"]
            self.assertIn("founder@realexample.com", payload["emails"])
            fetched_urls = {page["requested_url"] for page in payload["pages"]}
            self.assertIn("https://example.com/contact", fetched_urls)
            contact_page = next(page for page in payload["pages"] if page["requested_url"] == "https://example.com/contact")
            self.assertEqual(contact_page["title"], "Contact")

    def test_extract_store_locator_locations_drains_bullseye_json_list(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200, json_value: Any = None) -> None:
                self.text = text
                self.content = text.encode("utf-8")
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400
                self._json_value = json_value

            def json(self) -> Any:
                if self._json_value is not None:
                    return self._json_value
                return json.loads(self.text)

        locations = [
            {"Name": "Alpha - PA", "Address1": "10 MAIN ST", "City": "ERIE", "StateAbbr": "PA", "PostCode": "16501"},
            {"Name": "Beta - OH", "Address1": "20 MARKET ST", "City": "AKRON", "StateAbbr": "OH", "PostCode": "44308"},
        ]
        location_payload = json.dumps({"locations": locations})

        def fake_get(url: str, **kwargs: Any) -> Response:
            if url == "https://brand.example/locations":
                return Response(
                    '<a href="https://stores.example.com/local/list/example-store-near-me">Find a store</a>',
                    url,
                )
            if "GetInterfaceConfiguration" in url:
                if (kwargs.get("params") or {}).get("interfaceName") != "example-store-near-me":
                    return Response("", url, json_value={"clientId": None, "apiKey": None})
                return Response(
                    "",
                    "https://wswrapper.bullseyelocations.com/InterfaceConfiguration/GetInterfaceConfiguration?interfaceName=example-store-near-me",
                    json_value={
                        "clientId": 123,
                        "apiKey": "public-key",
                        "locationIdentifier": 1,
                        "countries": [{"id": 1, "name": "United States"}],
                    },
                )
            if "GetLocationList" in url:
                params = kwargs.get("params") or {}
                self.assertEqual(params["action"], "json")
                self.assertEqual(params["isSEO"], "true")
                self.assertEqual(params["isProxy"], "true")
                return Response(
                    json.dumps(location_payload),
                    "https://ws.bullseyelocations.com/RestSearch.svc/GetLocationList",
                    json_value=location_payload,
                )
            return Response("", url, status_code=404)

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
                            "load_skill('store_locators')\n"
                            "result = extract_store_locator_locations("
                            "'https://brand.example/locations', save_to='stores.json', include_locations=False)"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            payload = result.data["result"]
            self.assertTrue(payload["ok"])
            self.assertEqual(payload["provider"], "bullseye")
            self.assertEqual(payload["interface_name"], "example-store-near-me")
            self.assertEqual(payload["count"], 2)
            self.assertEqual(payload["sample"][0]["Name"], "Alpha - PA")
            self.assertNotIn("locations", payload)
            saved = Path(payload["path"])
            self.assertEqual(json.loads(saved.read_text(encoding="utf-8"))[1]["City"], "AKRON")

    def test_search_web_parses_duckduckgo_redirects_and_saves_empty_pages(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

            def raise_for_status(self) -> None:
                if not self.ok:
                    raise RuntimeError(f"HTTP {self.status_code}")

        def fake_get(url: str, **kwargs: Any) -> Response:
            if "bing.com/search" in url:
                return Response("<html><title>blocked</title></html>", url)
            if "duckduckgo.com/html" in url:
                html_page = (
                    '<div class="result">'
                    '<a class="result__a" href="/l/?uddg=https%3A%2F%2Fexample.com%2Falpha">Alpha result</a>'
                    '<a class="result__snippet">Useful snippet</a>'
                    "</div>"
                    '<div class="result">'
                    '<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fbeta">Beta result</a>'
                    "</div>"
                )
                return Response(html_page, url)
            raise AssertionError(url)

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
                        "code": "load_skill('search')\nresult = search_web('alpha beta', max_results=2, include_specialized=False)",
                    },
                )

            self.assertTrue(result.data["ok"])
            payload = result.data["result"]
            self.assertEqual([item["url"] for item in payload["results"]], ["https://example.com/alpha", "https://example.org/beta"])
            self.assertEqual(payload["attempts"][0]["parsed"], 0)
            self.assertTrue(Path(payload["attempts"][0]["raw_path"]).exists())

    def test_search_web_caps_per_engine_timeout(self) -> None:
        class Response:
            def __init__(self, text: str, url: str) -> None:
                self.text = text
                self.url = url
                self.status_code = 200
                self.ok = True

            def raise_for_status(self) -> None:
                return None

        calls: list[tuple[str, float]] = []

        def fake_get(url: str, **kwargs: Any) -> Response:
            calls.append((url, kwargs["timeout"]))
            if "r.jina.ai/http://https://www.google.com/search" in url:
                return Response("[Example](https://example.com/result)\n\nUseful snippet", url)
            return Response("<html><title>empty</title></html>", url)

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
                        "code": "load_skill('search')\nresult = search_web('slow search query', max_results=1, timeout=30, include_specialized=False)",
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["results"][0]["url"], "https://example.com/result")
            self.assertEqual([timeout for _, timeout in calls[:4]], [6.0, 6.0, 6.0, 6.0])
            self.assertEqual(calls[4][1], 12.0)

    def test_search_web_filters_reader_image_results(self) -> None:
        class Response:
            def __init__(self, text: str, url: str) -> None:
                self.text = text
                self.url = url
                self.status_code = 200
                self.ok = True

            def raise_for_status(self) -> None:
                return None

        def fake_get(url: str, **kwargs: Any) -> Response:
            if "r.jina.ai/http://https://www.google.com/search" in url:
                return Response(
                    "[Image 1](https://encrypted-tbn1.gstatic.com/images?q=abc)\n"
                    "[Favicon](https://encrypted-tbn2.gstatic.com/faviconV2?url=https://example.com)\n"
                    "[Useful](https://example.com/useful)",
                    url,
                )
            return Response("<html><title>empty</title></html>", url)

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
                        "code": "load_skill('search')\nresult = search_web('image-heavy query', max_results=2, include_specialized=False)",
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertEqual([item["url"] for item in result.data["result"]["results"]], ["https://example.com/useful"])

    def test_search_web_prioritizes_exact_cve_records(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": "load_skill('search')\nresult = search_web('CVE-2020-8166 Rails details', max_results=4, include_specialized=False)",
                },
            )

            self.assertTrue(result.data["ok"])
            payload = result.data["result"]
            urls = [item["url"] for item in payload["results"]]
            self.assertEqual(urls[0], "https://nvd.nist.gov/vuln/detail/CVE-2020-8166")
            self.assertEqual(urls[1], "https://www.cve.org/CVERecord?id=CVE-2020-8166")
            self.assertIn("/cves/2020/8xxx/CVE-2020-8166.json", urls[3])
            self.assertEqual(payload["attempts"][0]["source"], "cve_records")

    def test_search_web_prioritizes_fcc_grantee_records(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            result = tool(
                ctx,
                {
                    "headless": True,
                    "code": "load_skill('search')\nresult = search_web('site:fccid.io 2ACAH BCE FCC grantee code', max_results=4, include_specialized=False)",
                },
            )

            self.assertTrue(result.data["ok"])
            payload = result.data["result"]
            urls = [item["url"] for item in payload["results"]]
            self.assertEqual(urls[:4], ["https://fccid.io/2ACAH/", "https://fccid.io/company.php?grantee=2ACAH", "https://fccid.io/BCE/", "https://fccid.io/company.php?grantee=BCE"])
            self.assertEqual(payload["attempts"][0]["source"], "fcc_grantee_records")

    def test_search_web_uses_pubmed_fallback_when_page_search_is_empty(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200, payload: Any = None) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400
                self._payload = payload

            def json(self) -> Any:
                return self._payload if self._payload is not None else json.loads(self.text)

            def raise_for_status(self) -> None:
                if not self.ok:
                    raise RuntimeError(f"HTTP {self.status_code}")

        def fake_get(url: str, **kwargs: Any) -> Response:
            if "w/api.php" in url:
                return Response("[]", url, payload=["query", [], [], []])
            if "esearch.fcgi" in url:
                return Response("{}", url, payload={"esearchresult": {"idlist": ["12345"]}})
            if "esummary.fcgi" in url:
                return Response("{}", url, payload={"result": {"12345": {"title": "Important Hafnia paper", "source": "PubMed", "pubdate": "2026"}}})
            if "crossref.org" in url:
                return Response("{}", url, payload={"message": {"items": []}})
            return Response("<html><title>empty</title></html>", url)

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", side_effect=fake_get):
                result = tool(ctx, {"headless": True, "code": "load_skill('search')\nresult = search_web('Hafnia alvei animals', max_results=1)"})

            self.assertTrue(result.data["ok"])
            payload = result.data["result"]
            self.assertEqual(payload["results"][0]["title"], "Important Hafnia paper")
            self.assertEqual(payload["results"][0]["url"], "https://pubmed.ncbi.nlm.nih.gov/12345/")

    def test_search_web_does_not_use_scholarly_fallbacks_for_commerce_query_by_default(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200, payload: Any = None) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400
                self._payload = payload

            def json(self) -> Any:
                return self._payload if self._payload is not None else json.loads(self.text)

            def raise_for_status(self) -> None:
                if not self.ok:
                    raise RuntimeError(f"HTTP {self.status_code}")

        called_urls: list[str] = []

        def fake_get(url: str, **kwargs: Any) -> Response:
            called_urls.append(url)
            if "eutils.ncbi.nlm.nih.gov" in url or "api.crossref.org" in url:
                raise AssertionError(f"specialized fallback should not be used for commerce query: {url}")
            return Response("<html><title>empty</title></html>", url, payload=["query", [], [], []] if "w/api.php" in url else None)

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
                        "code": "load_skill('search')\nresult = search_web('site:kaufland.de Nahrungsergaenzungsmittel bestseller', max_results=2)",
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertFalse(result.data["result"]["results"])
            self.assertFalse(any("eutils.ncbi.nlm.nih.gov" in url or "api.crossref.org" in url for url in called_urls))

    def test_search_web_does_not_treat_people_names_as_scientific_names(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200, payload: Any = None) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400
                self._payload = payload

            def json(self) -> Any:
                return self._payload if self._payload is not None else json.loads(self.text)

            def raise_for_status(self) -> None:
                if not self.ok:
                    raise RuntimeError(f"HTTP {self.status_code}")

        called_urls: list[str] = []

        def fake_get(url: str, **kwargs: Any) -> Response:
            called_urls.append(url)
            if "eutils.ncbi.nlm.nih.gov" in url or "api.crossref.org" in url:
                raise AssertionError(f"specialized fallback should not be used for people/company query: {url}")
            return Response("<html><title>empty</title></html>", url, payload=["query", [], [], []] if "w/api.php" in url else None)

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
                        "code": "load_skill('search')\nresult = search_web('Pursuit startup government contracts website Mike Vichich', max_results=2)",
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertFalse(result.data["result"]["results"])
            self.assertFalse(any("eutils.ncbi.nlm.nih.gov" in url or "api.crossref.org" in url for url in called_urls))

    def test_search_web_parses_brave_snippets_without_footer_links(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

            def raise_for_status(self) -> None:
                if not self.ok:
                    raise RuntimeError(f"HTTP {self.status_code}")

        brave_html = (
            '<div class="snippet">'
            '<a href="https://www.kaufland.de/product/459358076/">'
            'Kaufland kaufland.de › product › 459358076 Kijimea K53 Advance</a>'
            '<p>Kijimea K53 Advance Kapseln 84 St Nahrungsergänzungsmittel</p>'
            "</div>"
            '<footer><a href="https://brave.com/download/">Brave Browser</a></footer>'
        )

        def fake_get(url: str, **kwargs: Any) -> Response:
            if "search.brave.com" in url:
                return Response(brave_html, url)
            return Response("<html><title>empty</title></html>", url)

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
                        "code": "load_skill('search')\nresult = search_web('site:kaufland.de/product supplements', max_results=3, include_specialized=False)",
                    },
                )

            self.assertTrue(result.data["ok"])
            urls = [item["url"] for item in result.data["result"]["results"]]
            self.assertEqual(urls, ["https://www.kaufland.de/product/459358076/"])

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
                            "load_skill('research')\n"
                            "load_skill('extraction')\n"
                            "load_skill('store_locators')\n"
                            "from browser_helpers import *\n"
                            "result = {"
                            "'text': fetch_text('https://example.com')[:5], "
                            "'structured': fetch_text_result('https://example.com')['source'], "
                            "'readable': fetch_readable_text('https://example.com')[:5], "
                            "'structured_readable': fetch_readable_text_result('https://example.com')['source'], "
                            "'many': fetch_many_text(['https://example.com'], save_to='bulk.json')['count'], "
                            "'blocks': extract_markdown_link_blocks('[A](https://example.com/a)')[0]['title'], "
                            "'locator': callable(extract_store_locator_locations), "
                            "'title': js('document.title')"
                            "}"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"]["text"], "hello")
            self.assertEqual(result.data["result"]["structured"], "direct")
            self.assertEqual(result.data["result"]["readable"], "hello")
            self.assertEqual(result.data["result"]["structured_readable"], "direct")
            self.assertEqual(result.data["result"]["many"], 1)
            self.assertEqual(result.data["result"]["blocks"], "A")
            self.assertTrue(result.data["result"]["locator"])
            self.assertEqual(result.data["result"]["title"], "Example Domain")

    def test_browser_helpers_search_web_is_sliceable_dict(self) -> None:
        class Response:
            def __init__(self, text: str, url: str, status_code: int = 200) -> None:
                self.text = text
                self.url = url
                self.status_code = status_code
                self.ok = 200 <= status_code < 400

        html = "<html><body><a href='https://example.com/a'><h2>Alpha Result</h2></a></body></html>"
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="python")
            tool = PythonBrowserTool(runtime_factory=lambda root_dir, headless: FakeRuntime(root_dir, headless))

            with patch("requests.get", return_value=Response(html, "https://www.bing.com/search?q=alpha")):
                result = tool(
                    ctx,
                    {
                        "headless": True,
                        "code": (
                            "load_skill('search')\n"
                            "from browser_helpers import *\n"
                            "res = search_web('alpha', include_specialized=False)\n"
                            "result = {"
                            "'is_dict': isinstance(res, dict), "
                            "'first_url': res['results'][0]['url'], "
                            "'preview': res[:80], "
                            "'structured': search_web_result('alpha', include_specialized=False)['query']"
                            "}"
                        ),
                    },
                )

            self.assertTrue(result.data["ok"])
            self.assertTrue(result.data["result"]["is_dict"])
            self.assertEqual(result.data["result"]["first_url"], "https://example.com/a")
            self.assertIn("Alpha Result", result.data["result"]["preview"])
            self.assertEqual(result.data["result"]["structured"], "alpha")

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
                            "load_skill('research')\n"
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

    def test_js_helper_accepts_timeout_alias(self) -> None:
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

            result = tool(ctx, {"headless": True, "code": "js('document.title', timeout=9); result = 'ok'"})

            self.assertTrue(result.data["ok"])
            self.assertEqual(runtime_holder["runtime"].last_js_timeout_s, 9)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
