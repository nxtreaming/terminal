from __future__ import annotations

import base64
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any, Dict, Optional
from unittest.mock import Mock, patch

from llm_browser.browser.runtime import BrowserRuntime


class JsRuntime(BrowserRuntime):
    def __init__(self, root_dir: Path, js_value: str) -> None:
        super().__init__(root_dir=root_dir)
        self.js_value = js_value

    def js(self, expression: str, await_promise: bool = False) -> Any:
        return self.js_value


class ScreenshotRuntime(BrowserRuntime):
    def __init__(self, root_dir: Path) -> None:
        super().__init__(root_dir=root_dir)
        self.target = {"url": "https://fallback.example", "title": "Fallback"}

    def cdp(
        self,
        method: str,
        params: Optional[Dict[str, Any]] = None,
        session_id: Optional[str] = None,
    ) -> Dict[str, Any]:
        if method == "Page.captureScreenshot":
            return {"data": base64.b64encode(b"png-bytes").decode("ascii")}
        return {}

    def page_info(self) -> Dict[str, Any]:
        raise RuntimeError("document is not ready")


class NewTabRuntime(BrowserRuntime):
    def __init__(self, root_dir: Path) -> None:
        super().__init__(root_dir=root_dir, http_url="http://127.0.0.1:9222")
        self.attached_target: Optional[Dict[str, Any]] = None
        self.navigated_to: Optional[str] = None

    def attach_target(self, target: Dict[str, Any]) -> Dict[str, Any]:
        self.attached_target = target
        return target

    def navigate(self, url: str, wait: bool = True, timeout_s: float = 20.0) -> Dict[str, Any]:
        self.navigated_to = url
        return {"url": url, "wait": wait}


class SequenceRuntime(BrowserRuntime):
    def __init__(self, root_dir: Path, values: list[Any]) -> None:
        super().__init__(root_dir=root_dir)
        self.values = values
        self.expressions: list[str] = []

    def js(self, expression: str, await_promise: bool = False) -> Any:
        self.expressions.append(expression)
        if self.values:
            return self.values.pop(0)
        return False


class BrowserRuntimeTest(unittest.TestCase):
    def test_page_info_handles_missing_document_elements(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = JsRuntime(
                Path(tmp),
                json.dumps(
                    {
                        "url": "about:blank",
                        "title": "",
                        "w": 0,
                        "h": 0,
                        "sx": 0,
                        "sy": 0,
                        "pw": 0,
                        "ph": 0,
                    }
                ),
            )

            self.assertEqual(runtime.page_info()["url"], "about:blank")

    def test_screenshot_writes_artifact_when_page_info_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = ScreenshotRuntime(Path(tmp))

            image = runtime.screenshot("fallback", attach=True)

            self.assertTrue(Path(image.path).exists())
            self.assertEqual(image.url, "https://fallback.example")
            self.assertTrue(Path(image.path).with_suffix(".json").exists())

    def test_new_tab_explicitly_navigates_when_chrome_returns_blank_target(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = NewTabRuntime(Path(tmp))
            response = Mock(status_code=200)
            response.raise_for_status.return_value = None
            response.json.return_value = {"id": "target-1", "url": "about:blank", "webSocketDebuggerUrl": "ws://target"}

            with patch("llm_browser.browser.runtime.requests.put", return_value=response):
                target = runtime.new_tab("https://example.com")

            self.assertEqual(target["id"], "target-1")
            self.assertEqual(runtime.navigated_to, "https://example.com")

    def test_wait_until_polls_until_truthy(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = SequenceRuntime(Path(tmp), [False, "", "ready"])

            self.assertEqual(runtime.wait_until("window.ready", timeout_s=1, interval_s=0), "ready")

    def test_wait_for_selector_builds_selector_expression(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = SequenceRuntime(Path(tmp), [True])

            self.assertTrue(runtime.wait_for_selector("#accept", timeout_s=1))
            self.assertIn('document.querySelector("#accept")', runtime.expressions[0])


if __name__ == "__main__":
    raise SystemExit(unittest.main())
