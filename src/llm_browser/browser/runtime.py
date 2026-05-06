from __future__ import annotations

import base64
import json
import math
import os
import shutil
import time
from pathlib import Path
from typing import Any, Dict, List, Optional
from urllib.parse import quote

import requests

from llm_browser.browser.cdp import CdpClient, CdpConnectionError
from llm_browser.browser.chrome import ChromeProcess, start_chrome
from llm_browser.events.event import now_ms
from llm_browser.tool.result import ToolImage


class BrowserRuntime:
    def __init__(self, root_dir: Path, http_url: Optional[str] = None, preserve_profile: bool = False) -> None:
        self.root_dir = root_dir
        self.http_url = http_url
        self.preserve_profile = preserve_profile
        self.chrome: Optional[ChromeProcess] = None
        self.client: Optional[CdpClient] = None
        self.target: Optional[Dict[str, Any]] = None
        self._screenshot_index = 0

    @classmethod
    def start(cls, root_dir: Path, headless: bool = False) -> "BrowserRuntime":
        runtime = cls(root_dir=root_dir, preserve_profile=_env_bool("LLM_BROWSER_KEEP_CHROME_PROFILE", False))
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
        runtime = cls(root_dir=root_dir, http_url=http_url.rstrip("/"), preserve_profile=True)
        runtime.attach_first_page()
        return runtime

    def close(self) -> None:
        if self.client is not None:
            self.client.close()
            self.client = None
        if self.chrome is not None:
            profile_dir = self.chrome.config.user_data_dir
            self.chrome.stop()
            self.chrome = None
            if not self.preserve_profile:
                shutil.rmtree(profile_dir, ignore_errors=True)

    def version(self) -> Dict[str, Any]:
        return requests.get(f"{self.http_url}/json/version", timeout=5).json()

    def targets(self) -> List[Dict[str, Any]]:
        return requests.get(f"{self.http_url}/json/list", timeout=5).json()

    def tabs(self) -> List[Dict[str, Any]]:
        return [target for target in self.targets() if target.get("type") == "page"]

    def attach_first_page(self) -> Dict[str, Any]:
        pages = self.tabs()
        if not pages:
            return self.new_tab("about:blank")
        return self.attach_target(pages[0])

    def attach_tab(
        self,
        target_id: Optional[str] = None,
        index: Optional[int] = None,
        url_contains: Optional[str] = None,
    ) -> Dict[str, Any]:
        pages = self.tabs()
        if target_id is not None:
            for page in pages:
                if page.get("id") == target_id:
                    return self.attach_target(page)
            raise ValueError(f"page target not found: {target_id}")
        if url_contains is not None:
            for page in pages:
                if url_contains in str(page.get("url") or ""):
                    return self.attach_target(page)
            raise ValueError(f"page URL containing {url_contains!r} not found")
        if index is None:
            index = 0
        if index < 0 or index >= len(pages):
            raise IndexError(f"page index {index} out of range for {len(pages)} page(s)")
        return self.attach_target(pages[index])

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
        target = self.attach_target(response.json())
        target_url = str(target.get("url") or "")
        if url != "about:blank" and target_url in {"", "about:blank"}:
            self.navigate(url, wait=False)
        return target

    def cdp(self, method: str, params: Optional[Dict[str, Any]] = None, session_id: Optional[str] = None) -> Dict[str, Any]:
        if self.client is None:
            self.attach_first_page()
        assert self.client is not None
        try:
            return self.client.call(method, params=params, session_id=session_id)
        except CdpConnectionError:
            self._reattach_after_disconnect()
            assert self.client is not None
            return self.client.call(method, params=params, session_id=session_id)

    def _reattach_after_disconnect(self) -> None:
        if self.client is not None:
            self.client.close()
            self.client = None
        target_id = str((self.target or {}).get("id") or "")
        if target_id:
            for page in self.tabs():
                if page.get("id") == target_id:
                    self.attach_target(page)
                    return
        self.attach_first_page()

    def navigate(self, url: str, wait: bool = True, timeout_s: float = 20.0) -> Dict[str, Any]:
        result = self.cdp("Page.navigate", {"url": url})
        if wait:
            self.wait_for_load(timeout_s=timeout_s)
        return result

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

    def wait_until(self, expression: str, timeout_s: float = 20.0, interval_s: float = 0.25) -> Any:
        deadline = time.time() + timeout_s
        last_error: Optional[Exception] = None
        last_value: Any = None
        while time.time() < deadline:
            try:
                last_value = self.js(expression)
                if last_value:
                    return last_value
            except Exception as exc:
                last_error = exc
            time.sleep(interval_s)
        if last_error is not None:
            raise TimeoutError(f"condition did not become truthy before timeout: {last_error}") from last_error
        raise TimeoutError(f"condition did not become truthy before timeout; last value: {last_value!r}")

    def wait_for_selector(self, selector: str, timeout_s: float = 20.0, visible: bool = False) -> Any:
        expression = json.dumps(selector)
        if visible:
            return self.wait_until(
                "(() => {"
                f"const el = document.querySelector({expression});"
                "if (!el) return false;"
                "const rect = el.getBoundingClientRect();"
                "return !!(rect.width || rect.height || el.getClientRects().length);"
                "})()",
                timeout_s=timeout_s,
            )
        return self.wait_until(f"document.querySelector({expression}) !== null", timeout_s=timeout_s)

    def wait_for_text(self, text: str, timeout_s: float = 20.0) -> Any:
        needle = json.dumps(text)
        return self.wait_until(
            f"((document.body && document.body.innerText) || '').includes({needle})",
            timeout_s=timeout_s,
        )

    def page_info(self) -> Dict[str, Any]:
        raw = self.js(
            """
            (() => {
              const de = document.documentElement || {};
              const body = document.body || {};
              const pageWidth = de.scrollWidth || body.scrollWidth || innerWidth || 0;
              const pageHeight = de.scrollHeight || body.scrollHeight || innerHeight || 0;
              return JSON.stringify({
                url: location.href || '',
                title: document.title || '',
                w: innerWidth || 0,
                h: innerHeight || 0,
                sx: scrollX || 0,
                sy: scrollY || 0,
                pw: pageWidth,
                ph: pageHeight
              });
            })()
            """
        )
        return json.loads(raw or "{}")

    def visible_text(self, max_chars: int = 8000) -> str:
        text = self.js(
            "(() => document.body ? document.body.innerText : '')()",
            await_promise=False,
        )
        return str(text or "")[:max_chars]

    def links(self, limit: int = 200) -> List[Dict[str, str]]:
        raw = self.js(
            "JSON.stringify(Array.from(document.links).slice(0, arguments_limit).map(a => "
            "({text:(a.innerText||a.textContent||'').trim(), href:a.href, title:a.title||''})))".replace(
                "arguments_limit", str(int(limit))
            )
        )
        return json.loads(raw or "[]")

    def screenshot(self, label: str = "screenshot", attach: bool = True, full_page: bool = False) -> ToolImage:
        params: Dict[str, Any] = {"format": "png", "fromSurface": True}
        if full_page:
            params["captureBeyondViewport"] = True
            metrics = self.cdp("Page.getLayoutMetrics")
            size = metrics.get("cssContentSize") or metrics.get("contentSize") or {}
            width = max(1, int(math.ceil(float(size.get("width") or 1280))))
            height = max(1, int(math.ceil(float(size.get("height") or 900))))
            params["clip"] = {"x": 0, "y": 0, "width": width, "height": height, "scale": 1}
        result = self.cdp("Page.captureScreenshot", params)
        data = base64.b64decode(result["data"])

        self._screenshot_index += 1
        safe_label = "".join(ch if ch.isalnum() or ch in {"-", "_"} else "_" for ch in label).strip("_")
        if not safe_label:
            safe_label = "screenshot"
        path = self.root_dir / "screenshots" / f"{self._screenshot_index:03d}_{safe_label}.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

        info = self._safe_page_info()
        image = ToolImage(
            label=label,
            path=str(path),
            order=self._screenshot_index,
            ts_ms=now_ms(),
            url=str(info.get("url", "")),
            title=str(info.get("title", "")),
        )
        path.with_suffix(".json").write_text(json.dumps(image.to_dict(), indent=2) + "\n", encoding="utf-8")
        return image

    def _safe_page_info(self) -> Dict[str, Any]:
        try:
            return self.page_info()
        except Exception:
            return {
                "url": str((self.target or {}).get("url") or ""),
                "title": str((self.target or {}).get("title") or ""),
                "w": 0,
                "h": 0,
                "sx": 0,
                "sy": 0,
                "pw": 0,
                "ph": 0,
            }

    def click_at(self, x: float, y: float, button: str = "left", clicks: int = 1) -> None:
        base = {"x": x, "y": y, "button": button, "clickCount": clicks}
        self.cdp("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x, "y": y})
        self.cdp("Input.dispatchMouseEvent", {"type": "mousePressed", **base})
        self.cdp("Input.dispatchMouseEvent", {"type": "mouseReleased", **base})

    def type_text(self, text: str) -> None:
        self.cdp("Input.insertText", {"text": text})

    def press(self, key: str) -> None:
        event = _key_event(key)
        self.cdp("Input.dispatchKeyEvent", {"type": "keyDown", **event})
        self.cdp("Input.dispatchKeyEvent", {"type": "keyUp", **event})

    def scroll(self, dx: float = 0, dy: float = 500, x: float = 500, y: float = 500) -> None:
        self.cdp("Input.dispatchMouseEvent", {"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy})


def _key_event(key: str) -> Dict[str, Any]:
    common = {
        "Enter": ("Enter", "Enter", 13),
        "Escape": ("Escape", "Escape", 27),
        "Backspace": ("Backspace", "Backspace", 8),
        "Tab": ("Tab", "Tab", 9),
        "ArrowDown": ("ArrowDown", "ArrowDown", 40),
        "ArrowUp": ("ArrowUp", "ArrowUp", 38),
        "ArrowLeft": ("ArrowLeft", "ArrowLeft", 37),
        "ArrowRight": ("ArrowRight", "ArrowRight", 39),
    }
    if key in common:
        key_name, code, vk = common[key]
        return {"key": key_name, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk}
    if len(key) == 1:
        vk = ord(key.upper())
        return {"key": key, "text": key, "code": f"Key{key.upper()}", "windowsVirtualKeyCode": vk}
    return {"key": key, "code": key}


def _env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.lower() in {"1", "true", "yes", "on"}
