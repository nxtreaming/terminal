from __future__ import annotations

import tempfile
import unittest
import os
from pathlib import Path
from typing import Any, Dict

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

    def js(self, expression: str, await_promise: bool = False) -> Any:
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


if __name__ == "__main__":
    raise SystemExit(unittest.main())
