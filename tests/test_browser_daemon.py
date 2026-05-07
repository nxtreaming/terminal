from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from llm_browser.browser.daemon_client import DaemonBrowserRuntime, _daemon_matches, ensure_daemon
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

    def test_daemon_runtime_surfaces_failed_call_without_hidden_restart(self) -> None:
        runtime = DaemonBrowserRuntime("demo", Path("/tmp/demo"), headless=True, backend="chromium")

        with patch("llm_browser.browser.daemon_client.request", side_effect=RuntimeError("stale")) as request, patch(
            "llm_browser.browser.daemon_client.ensure_daemon"
        ) as ensure:
            with self.assertRaises(RuntimeError):
                runtime.page_info()

        ensure.assert_not_called()
        self.assertEqual(request.call_count, 1)

    def test_daemon_runtime_cdp_payload_has_no_retry_field(self) -> None:
        runtime = DaemonBrowserRuntime("demo", Path("/tmp/demo"), headless=True, backend="chromium")

        with patch("llm_browser.browser.daemon_client.request", return_value={"result": {"ok": True}}) as request:
            result = runtime.cdp("Browser.getVersion", timeout_s=2)

        self.assertEqual(result, {"ok": True})
        payload = request.call_args.args[1]
        self.assertEqual(payload["op"], "cdp")
        self.assertEqual(payload["method"], "Browser.getVersion")
        self.assertNotIn("retry", payload)

    def test_daemon_identity_match_includes_root_backend_and_headless(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            status = {"ok": True, "root_dir": str(root), "backend": "chromium", "headless": True}

            self.assertTrue(_daemon_matches(status, root_dir=root, headless=True, backend="chromium"))
            self.assertFalse(_daemon_matches(status, root_dir=root, headless=False, backend="chromium"))
            self.assertFalse(_daemon_matches(status, root_dir=root, headless=True, backend="cdp"))
            self.assertFalse(_daemon_matches(status, root_dir=root / "other", headless=True, backend="chromium"))

    def test_ensure_daemon_reuses_matching_live_daemon(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            status = {"ok": True, "root_dir": str(root), "backend": "chromium", "headless": True}
            with patch("llm_browser.browser.daemon_client._daemon_status_payload", return_value=status), patch(
                "llm_browser.browser.daemon_client.subprocess.Popen"
            ) as popen:
                ensure_daemon(name="matched", root_dir=root, headless=True, backend="chromium")

        popen.assert_not_called()


if __name__ == "__main__":
    raise SystemExit(unittest.main())
