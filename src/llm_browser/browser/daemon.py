from __future__ import annotations

import argparse
import json
import os
import signal
import socket
import sys
import threading
import time
from pathlib import Path
from typing import Any, Dict, Optional, Sequence

from llm_browser.browser import daemon_ipc as ipc
from llm_browser.browser.runtime import BrowserRuntime, BrowserRuntimeOptions


CALLABLES = {
    "connection_info",
    "version",
    "targets",
    "tabs",
    "list_tabs",
    "attach_tab",
    "switch_tab",
    "current_tab",
    "current_cdp_session",
    "set_cdp_session",
    "ensure_real_tab",
    "iframe_target",
    "new_tab",
    "navigate",
    "js",
    "wait_for_load",
    "wait_until",
    "wait_for_selector",
    "wait_for_text",
    "wait_for_network_idle",
    "page_info",
    "visible_text",
    "links",
    "screenshot",
    "click_at",
    "fill_input",
    "type_text",
    "press",
    "press_key",
    "scroll",
    "drain_events",
    "pending_dialog_info",
    "recent_cdp_events",
    "recent_console_events",
    "recent_network_events",
    "recent_network_failures",
    "download_info",
    "save_browser_trace",
}


class BrowserDaemon:
    def __init__(self, name: str, root_dir: Path, headless: bool, backend: str = "chromium") -> None:
        self.name = ipc.normalize_name(name)
        self.root_dir = root_dir
        self.headless = headless
        self.backend = backend
        self.runtime: Optional[BrowserRuntime] = None
        self.stop = threading.Event()
        self.lock = threading.RLock()

    def start_runtime(self) -> None:
        options = BrowserRuntimeOptions.from_env()
        if self.backend:
            options = BrowserRuntimeOptions(**{**options.__dict__, "mode": self.backend})
        elif options.normalized_mode() == "daemon":
            options = BrowserRuntimeOptions(**{**options.__dict__, "mode": "chromium"})
        self.runtime = BrowserRuntime.start(root_dir=self.root_dir / "runtime", headless=self.headless, options=options)

    def close(self) -> None:
        if self.runtime is not None:
            self.runtime.close()
            self.runtime = None

    def handle(self, request: Dict[str, Any]) -> Dict[str, Any]:
        meta = request.get("meta")
        if meta == "ping":
            return {"pong": True, "pid": os.getpid(), "name": self.name}
        if meta == "status":
            return {
                "ok": True,
                "pid": os.getpid(),
                "name": self.name,
                "endpoint": ipc.endpoint(self.name),
                "root_dir": str(self.root_dir),
                "backend": self.backend,
                "headless": self.headless,
                "runtime": self.runtime.connection_info() if self.runtime else None,
            }
        if meta == "shutdown":
            self.stop.set()
            return {"ok": True}

        if self.runtime is None:
            raise RuntimeError("browser daemon runtime is not started")

        if request.get("op") == "cdp":
            with self.lock:
                result = self.runtime.cdp(
                    str(request["method"]),
                    params=request.get("params"),
                    session_id=request.get("session_id"),
                    timeout_s=request.get("timeout_s"),
                )
            return {"result": result}

        if request.get("op") == "call":
            name = str(request["name"])
            if name not in CALLABLES:
                raise RuntimeError(f"method is not daemon-callable: {name}")
            args = request.get("args") or []
            kwargs = request.get("kwargs") or {}
            if not isinstance(args, list) or not isinstance(kwargs, dict):
                raise RuntimeError("daemon call requires list args and object kwargs")
            method = getattr(self.runtime, name)
            with self.lock:
                value = method(*args, **kwargs)
            if hasattr(value, "to_dict"):
                value = value.to_dict()
            return {"result": value}

        raise RuntimeError(f"unknown daemon request: {request}")


def serve_posix(daemon: BrowserDaemon) -> None:
    path = ipc.sock_path(daemon.name)
    if path.exists():
        path.unlink()
    old_umask = os.umask(0o077)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        server.bind(str(path))
    finally:
        os.umask(old_umask)
    server.listen(64)
    server.settimeout(0.25)
    _serve_socket(daemon, server)


def serve_windows(daemon: BrowserDaemon) -> None:
    token = ipc.new_token()
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.bind(("127.0.0.1", 0))
    server.listen(64)
    server.settimeout(0.25)
    port = int(server.getsockname()[1])
    ipc.write_windows_port(daemon.name, port, token)
    daemon.expected_token = token  # type: ignore[attr-defined]
    _serve_socket(daemon, server)


def _serve_socket(daemon: BrowserDaemon, server: socket.socket) -> None:
    try:
        while not daemon.stop.is_set():
            try:
                client, _ = server.accept()
            except socket.timeout:
                continue
            threading.Thread(target=_handle_client, args=(daemon, client), daemon=True).start()
    finally:
        server.close()
        daemon.close()
        ipc.cleanup(daemon.name)


def _handle_client(daemon: BrowserDaemon, client: socket.socket) -> None:
    try:
        client.settimeout(120)
        data = b""
        while not data.endswith(b"\n"):
            chunk = client.recv(1 << 16)
            if not chunk:
                break
            data += chunk
        request = json.loads(data.decode("utf-8") or "{}")
        expected = getattr(daemon, "expected_token", None)
        if expected is not None and request.get("token") != expected:
            raise RuntimeError("unauthorized")
        response = daemon.handle(request)
    except Exception as exc:
        response = {"error": str(exc)}
    try:
        client.sendall((json.dumps(response, default=str, separators=(",", ":")) + "\n").encode("utf-8"))
    finally:
        client.close()


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="browser-use-terminal-daemon")
    parser.add_argument("--name", required=True)
    parser.add_argument("--root-dir", required=True)
    parser.add_argument("--headless", action="store_true")
    parser.add_argument("--backend", default=os.environ.get("LLM_BROWSER_DAEMON_BACKEND") or "chromium")
    args = parser.parse_args(argv)

    daemon = BrowserDaemon(args.name, Path(args.root_dir).expanduser(), headless=args.headless, backend=args.backend)
    daemon.root_dir.mkdir(parents=True, exist_ok=True)
    ipc.pid_path(daemon.name).write_text(str(os.getpid()), encoding="utf-8")

    def stop_signal(signum: int, frame: object) -> None:
        daemon.stop.set()

    signal.signal(signal.SIGTERM, stop_signal)
    if hasattr(signal, "SIGINT"):
        signal.signal(signal.SIGINT, stop_signal)

    try:
        daemon.start_runtime()
        if ipc.IS_WINDOWS:
            serve_windows(daemon)
        else:
            serve_posix(daemon)
    except Exception:
        daemon.close()
        ipc.cleanup(daemon.name)
        raise
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
