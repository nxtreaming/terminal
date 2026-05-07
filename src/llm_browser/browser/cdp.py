from __future__ import annotations

import json
import threading
import time
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

    def __init__(self, websocket_url: str, timeout_s: float = 30.0, suppress_origin: bool = False) -> None:
        self.websocket_url = websocket_url
        self.timeout_s = timeout_s
        self.suppress_origin = suppress_origin
        self._ws: Optional[websocket.WebSocket] = None
        self._next_id = 0
        self._lock = threading.Lock()
        self._events = []

    def connect(self) -> None:
        if self._ws is not None:
            return
        try:
            options = {"timeout": self.timeout_s}
            if self.suppress_origin:
                options["suppress_origin"] = True
            self._ws = websocket.create_connection(self.websocket_url, **options)
        except (websocket.WebSocketException, TimeoutError, OSError) as exc:
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
        timeout_s: Optional[float] = None,
        return_on_event: Optional[str] = None,
    ) -> Dict[str, Any]:
        self.connect()
        if self._ws is None:
            raise CdpError("CDP websocket is not connected")

        with self._lock:
            previous_timeout = None
            effective_timeout = self.timeout_s if timeout_s is None else timeout_s
            deadline = time.monotonic() + max(0.0, effective_timeout)
            timeout_changed = hasattr(self._ws, "settimeout")
            if timeout_changed:
                if hasattr(self._ws, "gettimeout"):
                    previous_timeout = self._ws.gettimeout()
                self._ws.settimeout(max(0.001, effective_timeout))
            try:
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
                except (websocket.WebSocketException, TimeoutError, OSError) as exc:
                    self.close()
                    raise CdpConnectionError(f"CDP websocket send failed: {exc}") from exc

                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise CdpConnectionError(f"CDP websocket receive timed out waiting for {method}")
                    if timeout_changed:
                        self._ws.settimeout(max(0.001, remaining))
                    try:
                        raw = self._ws.recv()
                    except (websocket.WebSocketTimeoutException, TimeoutError) as exc:
                        raise CdpConnectionError(f"CDP websocket receive timed out waiting for {method}: {exc}") from exc
                    except (websocket.WebSocketException, OSError) as exc:
                        self.close()
                        raise CdpConnectionError(f"CDP websocket receive failed: {exc}") from exc
                    if not raw:
                        self.close()
                        raise CdpConnectionError("CDP websocket closed")
                    payload = json.loads(raw)
                    if payload.get("id") != request_id:
                        self._events.append(payload)
                        if return_on_event and payload.get("method") == return_on_event:
                            return {}
                        continue
                    if "error" in payload:
                        error = payload["error"]
                        raise CdpError(f"{method} failed: {error}")
                    return payload.get("result") or {}
            finally:
                if timeout_changed and previous_timeout is not None and self._ws is not None and hasattr(self._ws, "settimeout"):
                    self._ws.settimeout(previous_timeout)

    def drain_events(self, timeout_s: float = 0.05, max_events: int = 1000):
        events = list(self._events)
        self._events.clear()
        if self._ws is None:
            return events
        deadline = time.monotonic() + max(0.0, timeout_s)
        with self._lock:
            previous_timeout = None
            timeout_changed = hasattr(self._ws, "settimeout")
            had_previous_timeout = False
            if timeout_changed:
                if hasattr(self._ws, "gettimeout"):
                    previous_timeout = self._ws.gettimeout()
                    had_previous_timeout = True
                self._ws.settimeout(min(max(timeout_s, 0.001), 0.05))
            try:
                while len(events) < max_events and time.monotonic() <= deadline:
                    try:
                        raw = self._ws.recv()
                    except IndexError:
                        break
                    except websocket.WebSocketTimeoutException:
                        break
                    except TimeoutError:
                        break
                    except (websocket.WebSocketException, OSError):
                        self.close()
                        break
                    if not raw:
                        self.close()
                        break
                    payload = json.loads(raw)
                    events.append(payload)
            finally:
                if timeout_changed and had_previous_timeout and self._ws is not None:
                    self._ws.settimeout(previous_timeout)
        return events
