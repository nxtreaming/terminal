from __future__ import annotations

import json
import threading
from typing import Any, Dict, Optional

import websocket


class CdpError(RuntimeError):
    pass


class CdpConnectionError(CdpError):
    pass


class CdpClient:
    """Synchronous CDP websocket client.

    It intentionally exposes raw CDP with minimal interpretation. Async CDP
    events are collected and can be drained by helpers, while command responses
    are matched by id.
    """

    def __init__(self, websocket_url: str, timeout_s: float = 30.0) -> None:
        self.websocket_url = websocket_url
        self.timeout_s = timeout_s
        self._ws: Optional[websocket.WebSocket] = None
        self._next_id = 0
        self._lock = threading.Lock()
        self._events = []

    def connect(self) -> None:
        if self._ws is not None:
            return
        try:
            self._ws = websocket.create_connection(self.websocket_url, timeout=self.timeout_s)
        except websocket.WebSocketException as exc:
            raise CdpConnectionError(f"CDP websocket connection failed: {exc}") from exc

    def close(self) -> None:
        ws = self._ws
        self._ws = None
        if ws is not None:
            ws.close()

    def call(
        self,
        method: str,
        params: Optional[Dict[str, Any]] = None,
        session_id: Optional[str] = None,
    ) -> Dict[str, Any]:
        self.connect()
        if self._ws is None:
            raise CdpError("CDP websocket is not connected")

        with self._lock:
            self._next_id += 1
            request_id = self._next_id
            message: Dict[str, Any] = {
                "id": request_id,
                "method": method,
                "params": params or {},
            }
            if session_id:
                message["sessionId"] = session_id
            try:
                self._ws.send(json.dumps(message, separators=(",", ":")))
            except websocket.WebSocketException as exc:
                self.close()
                raise CdpConnectionError(f"CDP websocket send failed: {exc}") from exc

            while True:
                try:
                    raw = self._ws.recv()
                except websocket.WebSocketException as exc:
                    self.close()
                    raise CdpConnectionError(f"CDP websocket receive failed: {exc}") from exc
                if not raw:
                    self.close()
                    raise CdpConnectionError("CDP websocket closed")
                payload = json.loads(raw)
                if payload.get("id") != request_id:
                    self._events.append(payload)
                    continue
                if "error" in payload:
                    error = payload["error"]
                    raise CdpError(f"{method} failed: {error}")
                return payload.get("result") or {}

    def drain_events(self):
        events = list(self._events)
        self._events.clear()
        return events
