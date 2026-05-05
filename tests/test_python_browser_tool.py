from __future__ import annotations

import tempfile
import unittest
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
        self.tabs = []

    def cdp(self, method: str, params=None, session_id=None) -> Dict[str, Any]:
        return {"method": method, "params": params or {}, "session_id": session_id}

    def new_tab(self, url: str = "about:blank") -> Dict[str, Any]:
        self.tabs.append(url)
        return {"url": url}

    def js(self, expression: str, await_promise: bool = False) -> Any:
        if expression == "document.title":
            return "Example Domain"
        return None

    def wait_for_load(self, timeout_s: float = 20.0) -> None:
        return None

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
                            "result = {'title': js('document.title')}",
                        ]
                    ),
                },
            )

            self.assertTrue(result.data["ok"])
            self.assertEqual(result.data["result"], {"title": "Example Domain"})
            self.assertEqual(result.images[0].label, "loaded")

            provider_content = result.to_provider_content()
            self.assertIsInstance(provider_content, list)
            self.assertEqual(provider_content[0]["type"], "input_text")
            self.assertEqual(provider_content[1]["type"], "input_image")

            image_events = [event for event in store.events.read(session.id) if event.type == "tool.image"]
            self.assertEqual(len(image_events), 1)
            self.assertEqual(image_events[0].payload["image"]["label"], "loaded")


if __name__ == "__main__":
    raise SystemExit(unittest.main())
