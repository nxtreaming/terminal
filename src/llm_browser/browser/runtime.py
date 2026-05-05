from __future__ import annotations

import base64
import json
import time
from pathlib import Path
from typing import Any, Dict, Optional
from urllib.parse import quote

import requests

from llm_browser.browser.cdp import CdpClient
from llm_browser.browser.chrome import ChromeProcess, start_chrome
from llm_browser.events.event import now_ms
from llm_browser.tool.result import ToolImage


class BrowserRuntime:
    def __init__(self, root_dir: Path, http_url: Optional[str] = None) -> None:
        self.root_dir = root_dir
        self.http_url = http_url
        self.chrome: Optional[ChromeProcess] = None
        self.client: Optional[CdpClient] = None
        self.target: Optional[Dict[str, Any]] = None
        self._screenshot_index = 0

    @classmethod
    def start(cls, root_dir: Path, headless: bool = False) -> "BrowserRuntime":
        runtime = cls(root_dir=root_dir)
        try:
            runtime.chrome = start_chrome(root_dir=root_dir, headless=headless)
            runtime.http_url = runtime.chrome.http_url
            runtime.attach_first_page()
        except BaseException:
            runtime.close()
            raise
        return runtime

    @classmethod
    def attach(cls, root_dir: Path, http_url: str) -> "BrowserRuntime":
        runtime = cls(root_dir=root_dir, http_url=http_url.rstrip("/"))
        runtime.attach_first_page()
        return runtime

    def close(self) -> None:
        if self.client is not None:
            self.client.close()
            self.client = None
        if self.chrome is not None:
            self.chrome.stop()
            self.chrome = None

    def version(self) -> Dict[str, Any]:
        return requests.get(f"{self.http_url}/json/version", timeout=5).json()

    def targets(self):
        return requests.get(f"{self.http_url}/json/list", timeout=5).json()

    def attach_first_page(self) -> Dict[str, Any]:
        pages = [target for target in self.targets() if target.get("type") == "page"]
        if not pages:
            return self.new_tab("about:blank")
        return self.attach_target(pages[0])

    def attach_target(self, target: Dict[str, Any]) -> Dict[str, Any]:
        websocket_url = target.get("webSocketDebuggerUrl")
        if not websocket_url:
            raise RuntimeError(f"target has no websocket URL: {target}")
        if self.client is not None:
            self.client.close()
        self.target = target
        self.client = CdpClient(websocket_url)
        self.client.connect()
        for domain in ("Page", "Runtime", "Network"):
            try:
                self.cdp(f"{domain}.enable")
            except Exception:
                pass
        return target

    def new_tab(self, url: str = "about:blank") -> Dict[str, Any]:
        encoded = quote(url, safe=":/?&=%#")
        response = requests.put(f"{self.http_url}/json/new?{encoded}", timeout=5)
        if response.status_code >= 400:
            response = requests.get(f"{self.http_url}/json/new?{encoded}", timeout=5)
        response.raise_for_status()
        return self.attach_target(response.json())

    def cdp(self, method: str, params: Optional[Dict[str, Any]] = None, session_id: Optional[str] = None) -> Dict[str, Any]:
        if self.client is None:
            self.attach_first_page()
        assert self.client is not None
        return self.client.call(method, params=params, session_id=session_id)

    def js(self, expression: str, await_promise: bool = False) -> Any:
        response = self.cdp(
            "Runtime.evaluate",
            {
                "expression": expression,
                "returnByValue": True,
                "awaitPromise": await_promise,
            },
        )
        result = response.get("result", {})
        details = response.get("exceptionDetails")
        if details or result.get("subtype") == "error":
            raise RuntimeError(f"JavaScript evaluation failed: {details or result}")
        if "value" in result:
            return result["value"]
        if "unserializableValue" in result:
            return result["unserializableValue"]
        return None

    def wait_for_load(self, timeout_s: float = 20.0) -> None:
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            try:
                state = self.js("document.readyState")
                if state in {"interactive", "complete"}:
                    return
            except Exception:
                pass
            time.sleep(0.1)
        raise TimeoutError("page did not reach interactive/complete readyState")

    def page_info(self) -> Dict[str, Any]:
        raw = self.js(
            "JSON.stringify({url:location.href,title:document.title,w:innerWidth,h:innerHeight,"
            "sx:scrollX,sy:scrollY,pw:document.documentElement.scrollWidth,ph:document.documentElement.scrollHeight})"
        )
        return json.loads(raw)

    def screenshot(self, label: str = "screenshot", attach: bool = True, full_page: bool = False) -> ToolImage:
        params: Dict[str, Any] = {"format": "png", "fromSurface": True}
        if full_page:
            params["captureBeyondViewport"] = True
        result = self.cdp("Page.captureScreenshot", params)
        data = base64.b64decode(result["data"])

        self._screenshot_index += 1
        safe_label = "".join(ch if ch.isalnum() or ch in {"-", "_"} else "_" for ch in label).strip("_")
        if not safe_label:
            safe_label = "screenshot"
        path = self.root_dir / "screenshots" / f"{self._screenshot_index:03d}_{safe_label}.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

        info = self.page_info()
        return ToolImage(
            label=label,
            path=str(path),
            order=self._screenshot_index,
            ts_ms=now_ms(),
            url=str(info.get("url", "")),
            title=str(info.get("title", "")),
        )

    def click_at(self, x: float, y: float, button: str = "left", clicks: int = 1) -> None:
        base = {"x": x, "y": y, "button": button, "clickCount": clicks}
        self.cdp("Input.dispatchMouseEvent", {"type": "mousePressed", **base})
        self.cdp("Input.dispatchMouseEvent", {"type": "mouseReleased", **base})

    def type_text(self, text: str) -> None:
        self.cdp("Input.insertText", {"text": text})

    def press(self, key: str) -> None:
        self.cdp("Input.dispatchKeyEvent", {"type": "keyDown", "key": key})
        self.cdp("Input.dispatchKeyEvent", {"type": "keyUp", "key": key})

    def scroll(self, dx: float = 0, dy: float = 500, x: float = 500, y: float = 500) -> None:
        self.cdp("Input.dispatchMouseEvent", {"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy})
