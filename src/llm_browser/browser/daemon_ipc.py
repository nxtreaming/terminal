from __future__ import annotations

import json
import os
import re
import secrets
import socket
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Dict, Optional, Tuple


IS_WINDOWS = sys.platform == "win32"
TMP_DIR = Path(os.environ.get("BROWSER_USE_TERMINAL_DAEMON_TMP") or (tempfile.gettempdir() if IS_WINDOWS else "/tmp"))
TMP_DIR.mkdir(parents=True, exist_ok=True)
NAME_RE = re.compile(r"\A[A-Za-z0-9_-]{1,64}\Z")


def normalize_name(name: str) -> str:
    if not NAME_RE.match(name or ""):
        raise ValueError(f"invalid daemon name {name!r}; use [A-Za-z0-9_-] up to 64 chars")
    return name


def sock_path(name: str) -> Path:
    return TMP_DIR / f"but-{normalize_name(name)}.sock"


def pid_path(name: str) -> Path:
    return TMP_DIR / f"but-{normalize_name(name)}.pid"


def log_path(name: str) -> Path:
    return TMP_DIR / f"but-{normalize_name(name)}.log"


def port_path(name: str) -> Path:
    return TMP_DIR / f"but-{normalize_name(name)}.port"


def endpoint(name: str) -> str:
    if not IS_WINDOWS:
        return str(sock_path(name))
    try:
        data = json.loads(port_path(name).read_text(encoding="utf-8"))
        return f"127.0.0.1:{data['port']}"
    except Exception:
        return f"tcp:but-{normalize_name(name)}"


def spawn_kwargs() -> Dict[str, Any]:
    if IS_WINDOWS:
        return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW}
    return {"start_new_session": True}


def connect(name: str, timeout_s: float = 5.0) -> Tuple[socket.socket, Optional[str]]:
    normalize_name(name)
    if not IS_WINDOWS:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(timeout_s)
        sock.connect(str(sock_path(name)))
        return sock, None
    data = json.loads(port_path(name).read_text(encoding="utf-8"))
    sock = socket.create_connection(("127.0.0.1", int(data["port"])), timeout=timeout_s)
    sock.settimeout(timeout_s)
    return sock, str(data["token"])


def request(name: str, payload: Dict[str, Any], timeout_s: float = 30.0) -> Dict[str, Any]:
    sock, token = connect(name, timeout_s=timeout_s)
    try:
        if token:
            payload = {**payload, "token": token}
        sock.sendall((json.dumps(payload, separators=(",", ":")) + "\n").encode("utf-8"))
        data = b""
        while not data.endswith(b"\n"):
            chunk = sock.recv(1 << 16)
            if not chunk:
                break
            data += chunk
        response = json.loads(data.decode("utf-8") or "{}")
    finally:
        sock.close()
    if not isinstance(response, dict):
        raise RuntimeError(f"daemon returned non-object response: {response!r}")
    if "error" in response:
        raise RuntimeError(str(response["error"]))
    return response


def ping(name: str, timeout_s: float = 1.0) -> bool:
    try:
        response = request(name, {"meta": "ping"}, timeout_s=timeout_s)
    except Exception:
        return False
    return response.get("pong") is True


def identify(name: str, timeout_s: float = 1.0) -> Optional[int]:
    try:
        response = request(name, {"meta": "ping"}, timeout_s=timeout_s)
    except Exception:
        return None
    pid = response.get("pid")
    return pid if type(pid) is int and 0 < pid < (1 << 31) else None


def cleanup(name: str) -> None:
    for path in (sock_path(name), pid_path(name), port_path(name)):
        try:
            path.unlink()
        except FileNotFoundError:
            pass


def write_windows_port(name: str, port: int, token: str) -> None:
    path = port_path(name)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps({"port": port, "token": token}), encoding="utf-8")
    os.replace(tmp, path)


def new_token() -> str:
    return secrets.token_hex(32)
