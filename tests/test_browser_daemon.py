from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from llm_browser.browser.daemon_client import DaemonBrowserRuntime
from llm_browser.browser.runtime import BrowserRuntime, BrowserRuntimeOptions


class BrowserDaemonTest(unittest.TestCase):
    def test_daemon_runtime_start_uses_named_backend(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            options = BrowserRuntimeOptions(mode="daemon", daemon_name="test-daemon", daemon_backend="cdp")
            with patch("llm_browser.browser.daemon_client.ensure_daemon") as ensure:
                runtime = DaemonBrowserRuntime.start(Path(tmp), headless=True, options=options)

        self.assertEqual(runtime.name, "test-daemon")
        ensure.assert_called_once_with(name="test-daemon", root_dir=Path(tmp) / "daemon", headless=True, backend="cdp")

    def test_browser_runtime_daemon_mode_delegates_to_daemon_runtime(self) -> None:
        sentinel = object()
        with tempfile.TemporaryDirectory() as tmp:
            options = BrowserRuntimeOptions(mode="daemon", daemon_name="delegate")
            with patch("llm_browser.browser.daemon_client.DaemonBrowserRuntime.start", return_value=sentinel) as start:
                runtime = BrowserRuntime.start(Path(tmp), headless=False, options=options)

        self.assertIs(runtime, sentinel)
        start.assert_called_once_with(root_dir=Path(tmp), headless=False, options=options)

    def test_daemon_runtime_proxies_screenshot_into_tool_image(self) -> None:
        payload = {
            "result": {
                "label": "loaded",
                "path": "/tmp/shot.png",
                "mime_type": "image/png",
                "detail": "auto",
                "order": 1,
                "ts_ms": 123,
                "url": "https://example.com",
                "title": "Example",
            }
        }
        runtime = DaemonBrowserRuntime("demo", Path("/tmp/demo"))

        with patch("llm_browser.browser.daemon_client.request", return_value=payload) as request:
            image = runtime.screenshot("loaded", attach=True)

        self.assertEqual(image.label, "loaded")
        request.assert_called_once()
        self.assertEqual(request.call_args.args[1]["name"], "screenshot")


if __name__ == "__main__":
    raise SystemExit(unittest.main())
