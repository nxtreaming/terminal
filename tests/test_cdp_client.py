from __future__ import annotations

import json
import unittest
from unittest.mock import patch

from llm_browser.browser.cdp import CdpClient, CdpConnectionError, CdpError


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


class TimeoutWebSocket(FakeWebSocket):
    def __init__(self):
        super().__init__([])

    def recv(self) -> str:
        raise TimeoutError("timed out")


class EventFloodWebSocket(FakeWebSocket):
    def __init__(self):
        super().__init__([])
        self.recv_count = 0

    def recv(self) -> str:
        self.recv_count += 1
        return json.dumps({"method": "Runtime.consoleAPICalled", "params": {"count": self.recv_count}})


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

    def test_call_treats_socket_timeout_as_non_destructive_connection_error(self) -> None:
        ws = TimeoutWebSocket()

        with patch("llm_browser.browser.cdp.websocket.create_connection", return_value=ws):
            client = CdpClient("ws://example")
            with self.assertRaises(CdpConnectionError):
                client.call("Runtime.evaluate")

        self.assertFalse(ws.closed)

    def test_call_times_out_even_when_events_keep_arriving(self) -> None:
        ws = EventFloodWebSocket()

        with patch("llm_browser.browser.cdp.websocket.create_connection", return_value=ws), patch(
            "llm_browser.browser.cdp.time.monotonic", side_effect=[0.0, 0.1, 0.2, 0.3]
        ):
            client = CdpClient("ws://example")
            with self.assertRaisesRegex(CdpConnectionError, "timed out waiting for Runtime.evaluate"):
                client.call("Runtime.evaluate", timeout_s=0.25)

        self.assertFalse(ws.closed)
        self.assertGreaterEqual(ws.recv_count, 2)

    def test_call_can_return_when_specific_event_arrives(self) -> None:
        ws = FakeWebSocket(
            [
                {"method": "Page.javascriptDialogOpening", "params": {"message": "Alert Token 123"}},
                {"id": 1, "result": {"late": True}},
            ]
        )

        with patch("llm_browser.browser.cdp.websocket.create_connection", return_value=ws):
            client = CdpClient("ws://example")
            result = client.call("Input.dispatchMouseEvent", return_on_event="Page.javascriptDialogOpening")

        self.assertEqual(result, {})
        self.assertFalse(ws.closed)
        self.assertEqual(client.drain_events()[0]["method"], "Page.javascriptDialogOpening")


if __name__ == "__main__":
    raise SystemExit(unittest.main())
