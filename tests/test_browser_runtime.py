from __future__ import annotations

import base64
import json
import os
import tempfile
import unittest
from pathlib import Path
from typing import Any, Dict, Optional
from unittest.mock import Mock, patch

from llm_browser.browser.cdp import CdpError
from llm_browser.browser.runtime import (
    BrowserRuntime,
    BrowserRuntimeOptions,
    DiscoveredCdpEndpoint,
    discover_real_browser_endpoint,
)


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
        self.last_params: Optional[Dict[str, Any]] = None

    def cdp(
        self,
        method: str,
        params: Optional[Dict[str, Any]] = None,
        session_id: Optional[str] = None,
        timeout_s: Optional[float] = None,
        retry: bool = True,
    ) -> Dict[str, Any]:
        self.last_timeout_s = timeout_s
        self.last_retry = retry
        self.last_params = params or {}
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


class EvalRuntime(BrowserRuntime):
    def __init__(self, root_dir: Path) -> None:
        super().__init__(root_dir=root_dir)
        self.last_params: Optional[Dict[str, Any]] = None

    def cdp(
        self,
        method: str,
        params: Optional[Dict[str, Any]] = None,
        session_id: Optional[str] = None,
    ) -> Dict[str, Any]:
        self.last_params = params or {}
        return {"result": {"value": "ok"}}


class RecordingRuntime(BrowserRuntime):
    def __init__(self, root_dir: Path) -> None:
        super().__init__(root_dir=root_dir)
        self.calls: list[tuple[str, Dict[str, Any]]] = []
        self.focused = True

    def cdp(
        self,
        method: str,
        params: Optional[Dict[str, Any]] = None,
        session_id: Optional[str] = None,
        timeout_s: Optional[float] = None,
        retry: bool = True,
    ) -> Dict[str, Any]:
        self.calls.append((method, params or {}))
        if method == "Runtime.evaluate":
            expression = str((params or {}).get("expression") or "")
            if "document.querySelector" in expression and ".focus()" in expression:
                return {"result": {"value": self.focused}}
            return {"result": {"value": None}}
        return {}


class DrainClient:
    def __init__(self, events: list[Dict[str, Any]]) -> None:
        self.events = list(events)

    def drain_events(self, timeout_s: float = 0.05, max_events: int = 1000):
        events = self.events[:max_events]
        self.events = self.events[max_events:]
        return events


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
            self.assertEqual(runtime.last_timeout_s, 8.0)
            self.assertFalse(runtime.last_retry)

    def test_screenshot_accepts_page_clip(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = ScreenshotRuntime(Path(tmp))

            runtime.screenshot("table", clip={"x": 10, "y": 20, "width": 300, "height": 120})

            self.assertEqual(runtime.last_params["clip"]["x"], 10)
            self.assertEqual(runtime.last_params["clip"]["y"], 20)
            self.assertEqual(runtime.last_params["clip"]["width"], 300)
            self.assertEqual(runtime.last_params["clip"]["height"], 120)
            self.assertTrue(runtime.last_params["captureBeyondViewport"])

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

    def test_start_attaches_to_env_http_cdp_endpoint(self) -> None:
        sentinel = object()
        with tempfile.TemporaryDirectory() as tmp:
            with patch.dict(os.environ, {"LLM_BROWSER_CDP_HTTP_URL": "http://127.0.0.1:9222"}, clear=False):
                with patch.object(BrowserRuntime, "attach", return_value=sentinel) as attach:
                    runtime = BrowserRuntime.start(Path(tmp), headless=True)

            self.assertIs(runtime, sentinel)
            attach.assert_called_once_with(root_dir=Path(tmp), http_url="http://127.0.0.1:9222")

    def test_attach_ws_can_drive_current_target_without_http_endpoint(self) -> None:
        client = Mock()
        client.call.return_value = {}
        client_cls = Mock(return_value=client)

        with tempfile.TemporaryDirectory() as tmp:
            with patch("llm_browser.browser.runtime.CdpClient", client_cls):
                runtime = BrowserRuntime.attach_ws(Path(tmp), "ws://remote/page")
                target = runtime.new_tab("https://example.com")

        client.connect.assert_called_once()
        self.assertEqual(target["id"], "external")
        client.call.assert_any_call("Page.enable", params=None, session_id=None, timeout_s=None)
        client.call.assert_any_call("Runtime.enable", params=None, session_id=None, timeout_s=None)
        client.call.assert_any_call("Network.enable", params=None, session_id=None, timeout_s=None)
        client.call.assert_any_call("Page.navigate", params={"url": "https://example.com"}, session_id=None, timeout_s=None)

    def test_attach_browser_level_ws_uses_page_session_for_page_commands(self) -> None:
        client = Mock()

        def call(method: str, params: Optional[Dict[str, Any]] = None, session_id: Optional[str] = None, timeout_s: Optional[float] = None):
            if method == "Target.getTargets":
                return {"targetInfos": [{"targetId": "page-1", "type": "page", "url": "about:blank"}]}
            if method == "Target.attachToTarget":
                return {"sessionId": "session-1"}
            if method == "Target.createTarget":
                return {"targetId": "page-2"}
            return {}

        client.call.side_effect = call
        client_cls = Mock(return_value=client)

        with tempfile.TemporaryDirectory() as tmp:
            with patch("llm_browser.browser.runtime.CdpClient", client_cls):
                runtime = BrowserRuntime.attach_ws(Path(tmp), "ws://remote/browser")
                runtime.navigate("https://example.com", wait=False)
                new_target = runtime.new_tab("https://new.example")

        self.assertTrue(runtime.browser_level_ws)
        self.assertEqual(runtime.default_session_id, "session-1")
        self.assertEqual(new_target["id"], "page-2")
        client.call.assert_any_call(
            "Page.navigate",
            params={"url": "https://example.com"},
            session_id="session-1",
            timeout_s=None,
        )
        client.call.assert_any_call("Target.createTarget", {"url": "about:blank"})
        client.call.assert_any_call(
            "Page.navigate",
            params={"url": "https://new.example"},
            session_id="session-1",
            timeout_s=None,
        )

    def test_attach_browser_level_ws_prefers_real_page_target(self) -> None:
        client = Mock()

        def call(method: str, params: Optional[Dict[str, Any]] = None, session_id: Optional[str] = None, timeout_s: Optional[float] = None):
            if method == "Target.getTargets":
                return {
                    "targetInfos": [
                        {"targetId": "blank", "type": "page", "url": "about:blank"},
                        {"targetId": "settings", "type": "page", "url": "chrome://settings"},
                        {"targetId": "real", "type": "page", "url": "https://example.com"},
                    ]
                }
            if method == "Target.attachToTarget":
                return {"sessionId": f"session-{params['targetId']}"}
            return {}

        client.call.side_effect = call

        with tempfile.TemporaryDirectory() as tmp:
            with patch("llm_browser.browser.runtime.CdpClient", Mock(return_value=client)):
                runtime = BrowserRuntime.attach_ws(Path(tmp), "ws://remote/browser")

        self.assertEqual(runtime.default_session_id, "session-real")
        client.call.assert_any_call("Target.attachToTarget", {"targetId": "real", "flatten": True})

    def test_start_cloud_creates_browser_attaches_ws_and_stops_on_close(self) -> None:
        client = Mock()

        def call(method: str, params: Optional[Dict[str, Any]] = None, session_id: Optional[str] = None, timeout_s: Optional[float] = None):
            if method == "Target.getTargets":
                raise CdpError("page websocket")
            return {}

        client.call.side_effect = call
        client_cls = Mock(return_value=client)
        post_response = Mock(content=b"{}")
        post_response.raise_for_status.return_value = None
        post_response.json.return_value = {"id": "browser-1", "wsUrl": "ws://cloud/page", "liveUrl": "https://live.example"}
        patch_response = Mock(content=b"{}")
        patch_response.raise_for_status.return_value = None
        patch_response.json.return_value = {}

        def request(method: str, url: str, **kwargs: Any):
            if method == "POST":
                return post_response
            if method == "PATCH":
                return patch_response
            raise AssertionError(method)

        with tempfile.TemporaryDirectory() as tmp:
            options = BrowserRuntimeOptions(mode="cloud", cloud_api_key="key", cloud_timeout=30)
            with patch("llm_browser.browser.runtime.CdpClient", client_cls), patch(
                "llm_browser.browser.runtime.requests.request", side_effect=request
            ) as request_mock:
                runtime = BrowserRuntime.start(Path(tmp), options=options)
                self.assertEqual(runtime.mode, "cloud")
                self.assertEqual(runtime.cloud_live_url, "https://live.example")
                runtime.close()

        client_cls.assert_called_once_with("ws://cloud/page")
        request_mock.assert_any_call(
            "POST",
            "https://api.browser-use.com/api/v3/browsers",
            json={
                "timeout": 30,
                "browserScreenWidth": 1280,
                "browserScreenHeight": 900,
            },
            timeout=60,
            headers={"X-Browser-Use-API-Key": "key", "Content-Type": "application/json"},
        )
        request_mock.assert_any_call(
            "PATCH",
            "https://api.browser-use.com/api/v3/browsers/browser-1",
            json={"action": "stop"},
            timeout=60,
            headers={"X-Browser-Use-API-Key": "key", "Content-Type": "application/json"},
        )

    def test_runtime_drains_console_and_network_failures(self) -> None:
        events = [
            {
                "method": "Runtime.consoleAPICalled",
                "params": {"type": "error", "args": [{"type": "string", "value": "boom"}]},
            },
            {
                "method": "Network.responseReceived",
                "params": {"requestId": "1", "response": {"status": 404, "statusText": "Not Found", "url": "https://e.test/missing"}},
            },
            {
                "method": "Network.loadingFailed",
                "params": {"requestId": "2", "errorText": "net::ERR_ABORTED", "canceled": True},
            },
        ]
        with tempfile.TemporaryDirectory() as tmp:
            runtime = BrowserRuntime(Path(tmp))
            runtime.client = DrainClient(events)  # type: ignore[assignment]

            console = runtime.recent_console_events()
            failures = runtime.recent_network_failures()

        self.assertEqual(console[0]["text"], "boom")
        self.assertEqual(failures[0]["status"], 404)
        self.assertEqual(failures[1]["errorText"], "net::ERR_ABORTED")

    def test_page_info_returns_pending_dialog_without_js(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = BrowserRuntime(Path(tmp))
            runtime.client = DrainClient(
                [
                    {
                        "method": "Page.javascriptDialogOpening",
                        "params": {"type": "alert", "message": "Confirm"},
                    }
                ]
            )  # type: ignore[assignment]

            info = runtime.page_info()

        self.assertEqual(info["dialog"]["message"], "Confirm")

    def test_wait_for_network_idle_filters_other_sessions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = BrowserRuntime(Path(tmp))
            runtime.default_session_id = "active"
            runtime.client = DrainClient(
                [
                    {
                        "method": "Network.requestWillBeSent",
                        "session_id": "background",
                        "params": {"requestId": "bg"},
                    }
                ]
            )  # type: ignore[assignment]

            self.assertTrue(runtime.wait_for_network_idle(timeout_s=0.5, idle_ms=10))

    def test_fill_input_uses_real_key_events_and_framework_events(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = RecordingRuntime(Path(tmp))

            runtime.fill_input("#email", "ab")

        event_types = [params.get("type") for method, params in runtime.calls if method == "Input.dispatchKeyEvent"]
        self.assertIn("rawKeyDown", event_types)
        self.assertIn("char", event_types)
        expressions = [params.get("expression", "") for method, params in runtime.calls if method == "Runtime.evaluate"]
        self.assertTrue(any("new Event('input'" in expression for expression in expressions))

    def test_attach_first_page_prefers_real_page_targets(self) -> None:
        class AttachRuntime(BrowserRuntime):
            def __init__(self, root_dir: Path) -> None:
                super().__init__(root_dir=root_dir)
                self.attached: Optional[Dict[str, Any]] = None

            def tabs(self) -> list[Dict[str, Any]]:
                return [
                    {"id": "chrome", "type": "page", "url": "chrome://new-tab-page"},
                    {"id": "real", "type": "page", "url": "https://example.com"},
                ]

            def attach_target(self, target: Dict[str, Any]) -> Dict[str, Any]:
                self.attached = target
                return target

        with tempfile.TemporaryDirectory() as tmp:
            runtime = AttachRuntime(Path(tmp))

            target = runtime.attach_first_page()

        self.assertEqual(target["id"], "real")

    def test_download_info_lists_files_and_download_events(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = BrowserRuntime(Path(tmp))
            runtime.client = DrainClient(
                [
                    {
                        "method": "Browser.downloadWillBegin",
                        "params": {"guid": "g1", "suggestedFilename": "report.csv"},
                    }
                ]
            )  # type: ignore[assignment]
            (runtime.downloads_dir / "report.csv").write_text("a,b\n1,2\n", encoding="utf-8")

            info = runtime.download_info()

        self.assertEqual(info["files"][0]["name"], "report.csv")
        self.assertTrue(info["files"][0]["complete"])
        self.assertEqual(info["events"][0]["method"], "Browser.downloadWillBegin")

    def test_save_browser_trace_writes_drained_events(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = BrowserRuntime(Path(tmp))
            runtime.client = DrainClient([{"method": "Log.entryAdded", "params": {"entry": {"text": "hi"}}}])  # type: ignore[assignment]

            trace = runtime.save_browser_trace("checkout trace")

            payload = json.loads(Path(trace["path"]).read_text(encoding="utf-8"))

        self.assertEqual(trace["event_count"], 1)
        self.assertEqual(payload["events"][0]["method"], "Log.entryAdded")

    def test_discover_real_browser_uses_devtools_active_port_ws_when_http_discovery_is_blocked(self) -> None:
        response = Mock(status_code=404)
        with tempfile.TemporaryDirectory() as tmp:
            profile = Path(tmp)
            (profile / "DevToolsActivePort").write_text("9333\n/devtools/browser/abc\n", encoding="utf-8")
            with patch("llm_browser.browser.runtime.requests.get", return_value=response):
                endpoint = discover_real_browser_endpoint(profile_dirs=[profile], probe_ports=[], timeout_s=0)

        self.assertEqual(endpoint.websocket_url, "ws://127.0.0.1:9333/devtools/browser/abc")
        self.assertIsNone(endpoint.http_url)

    def test_start_real_uses_discovered_websocket_endpoint(self) -> None:
        sentinel = object()
        endpoint = DiscoveredCdpEndpoint(websocket_url="ws://real/browser", source="test")
        with tempfile.TemporaryDirectory() as tmp:
            options = BrowserRuntimeOptions(mode="real")
            with patch("llm_browser.browser.runtime.discover_real_browser_endpoint", return_value=endpoint), patch.object(
                BrowserRuntime, "attach_ws", return_value=sentinel
            ) as attach_ws:
                runtime = BrowserRuntime.start(Path(tmp), options=options)

        self.assertIs(runtime, sentinel)
        attach_ws.assert_called_once_with(root_dir=Path(tmp), websocket_url="ws://real/browser", mode="real")

    def test_js_uses_repl_mode_by_default_for_repeated_snippets(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = EvalRuntime(Path(tmp))

            self.assertEqual(runtime.js("let f = 1; f", await_promise=True), "ok")

            self.assertEqual(runtime.last_params["expression"], "let f = 1; f")
            self.assertTrue(runtime.last_params["awaitPromise"])
            self.assertTrue(runtime.last_params["replMode"])
            self.assertFalse(runtime.last_params["userGesture"])

    def test_js_disables_repl_mode_for_promise_values(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = EvalRuntime(Path(tmp))

            runtime.js("(async () => ({status: 200}))()", await_promise=True)

            self.assertFalse(runtime.last_params["replMode"])

    def test_js_allows_forcing_repl_mode_for_promise_values(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = EvalRuntime(Path(tmp))

            runtime.js("(async () => ({status: 200}))()", await_promise=True, repl_mode=True)

            self.assertTrue(runtime.last_params["replMode"])

    def test_js_allows_exact_runtime_evaluate_mode(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runtime = EvalRuntime(Path(tmp))

            runtime.js("document.title", repl_mode=False, user_gesture=True)

            self.assertFalse(runtime.last_params["replMode"])
            self.assertTrue(runtime.last_params["userGesture"])

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
