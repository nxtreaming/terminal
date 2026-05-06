from __future__ import annotations

import json
import unittest
from unittest.mock import patch

from llm_browser.browser.cdp import CdpClient, CdpError


class FakeWebSocket:
    def __init__(self, replies):
        self.replies = list(replies)
        self.sent = []
        self.closed = False
        self.timeout = 30.0

    def send(self, payload: str) -> None:
        self.sent.append(json.loads(payload))

    def recv(self) -> str:
        return json.dumps(self.replies.pop(0))

    def settimeout(self, timeout: float) -> None:
        self.timeout = timeout

    def gettimeout(self) -> float:
        return self.timeout

    def close(self) -> None:
        self.closed = True


class CdpClientTest(unittest.TestCase):
    def test_call_matches_response_and_drains_events(self) -> None:
        ws = FakeWebSocket(
            [
                {"method": "Runtime.consoleAPICalled", "params": {"type": "log"}},
                {"id": 1, "result": {"ok": True}},
            ]
        )

        with patch("llm_browser.browser.cdp.websocket.create_connection", return_value=ws):
            client = CdpClient("ws://example")
            result = client.call("Runtime.evaluate", {"expression": "1+1"})

        self.assertEqual(result, {"ok": True})
        self.assertEqual(ws.sent[0]["method"], "Runtime.evaluate")
        self.assertEqual(client.drain_events()[0]["method"], "Runtime.consoleAPICalled")

    def test_call_raises_cdp_error(self) -> None:
        ws = FakeWebSocket([{"id": 1, "error": {"message": "bad"}}])

        with patch("llm_browser.browser.cdp.websocket.create_connection", return_value=ws):
            client = CdpClient("ws://example")
            with self.assertRaises(CdpError):
                client.call("Bad.method")

    def test_call_uses_per_call_timeout_and_restores_default(self) -> None:
        ws = FakeWebSocket([{"id": 1, "result": {"ok": True}}])

        with patch("llm_browser.browser.cdp.websocket.create_connection", return_value=ws):
            client = CdpClient("ws://example")
            self.assertEqual(client.call("Runtime.evaluate", timeout_s=4), {"ok": True})

        self.assertEqual(ws.timeout, 30.0)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
