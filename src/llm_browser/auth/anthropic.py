from __future__ import annotations

import base64
import hashlib
import json
import os
import queue
import secrets
import threading
import time
import urllib.parse
import webbrowser
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import Any, Callable, Dict, Optional

import requests


CLIENT_ID = base64.b64decode("OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl").decode("ascii")
AUTHORIZE_URL = "https://claude.ai/oauth/authorize"
TOKEN_URL = "https://platform.claude.com/v1/oauth/token"
CALLBACK_HOST = "127.0.0.1"
CALLBACK_PORT = 53692
CALLBACK_PATH = "/callback"
REDIRECT_URI = f"http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}"
SCOPES = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"


def login_anthropic_oauth(
    *,
    open_browser: bool = True,
    code: Optional[str] = None,
    timeout_s: float = 900.0,
    on_url: Optional[Callable[[str], None]] = None,
    on_status: Optional[Callable[[str], None]] = None,
    prompt_on_timeout: bool = True,
) -> Dict[str, Any]:
    verifier, challenge = _generate_pkce()
    parsed = _parse_authorization_input(code or "")
    if parsed.get("code"):
        return _exchange_authorization_code(parsed["code"], parsed.get("state") or verifier, verifier, REDIRECT_URI)

    callback = _CallbackServer(expected_state=verifier)
    callback.start()
    try:
        params = urllib.parse.urlencode(
            {
                "code": "true",
                "client_id": CLIENT_ID,
                "response_type": "code",
                "redirect_uri": REDIRECT_URI,
                "scope": SCOPES,
                "code_challenge": challenge,
                "code_challenge_method": "S256",
                "state": verifier,
            }
        )
        url = f"{AUTHORIZE_URL}?{params}"
        if on_url is not None:
            on_url(url)
        else:
            print("Open this URL to login with Anthropic Claude Code:\n")
            print(url)
        if on_status is not None:
            on_status("Waiting for Anthropic browser callback...")
        else:
            print("\nWaiting for browser callback...")
        if open_browser:
            webbrowser.open(url)
        result = callback.wait(timeout_s=timeout_s)
        if result is None:
            if not prompt_on_timeout:
                raise TimeoutError("Anthropic OAuth login timed out waiting for browser callback")
            manual = input("Paste the authorization code or final redirect URL: ")
            result = _parse_authorization_input(manual)
        auth_code = result.get("code")
        state = result.get("state") or verifier
        if not auth_code:
            raise RuntimeError("Missing Anthropic authorization code")
        if state != verifier:
            raise RuntimeError("Anthropic OAuth state mismatch")
        return _exchange_authorization_code(auth_code, state, verifier, REDIRECT_URI)
    finally:
        callback.stop()


def refresh_anthropic_token(refresh_token: str) -> Dict[str, Any]:
    if not refresh_token:
        raise RuntimeError("Missing Anthropic refresh token")
    response = _post_token(
        {
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }
    )
    return _credential_from_token_response(response)


def is_anthropic_oauth_token(token: str) -> bool:
    return token.startswith("sk-ant-oat") or "sk-ant-oat" in token


def _exchange_authorization_code(code: str, state: str, verifier: str, redirect_uri: str) -> Dict[str, Any]:
    response = _post_token(
        {
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        }
    )
    return _credential_from_token_response(response)


def _post_token(body: Dict[str, str]) -> Dict[str, Any]:
    response = requests.post(
        TOKEN_URL,
        headers={"Content-Type": "application/json", "Accept": "application/json"},
        json=body,
        timeout=30,
    )
    if response.status_code >= 400:
        raise RuntimeError(f"Anthropic OAuth token request failed: HTTP {response.status_code}: {response.text[:1000]}")
    try:
        payload = response.json()
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Anthropic OAuth token response was not JSON: {response.text[:1000]}") from exc
    if not isinstance(payload, dict):
        raise RuntimeError("Anthropic OAuth token response was not an object")
    return payload


def _credential_from_token_response(payload: Dict[str, Any]) -> Dict[str, Any]:
    access = payload.get("access_token")
    refresh = payload.get("refresh_token")
    expires_in = int(payload.get("expires_in") or 0)
    if not access or not refresh or expires_in <= 0:
        raise RuntimeError("Anthropic OAuth token response did not include access, refresh, and expires_in")
    return {
        "access": str(access),
        "refresh": str(refresh),
        "expires": int(time.time() * 1000) + expires_in * 1000 - 5 * 60 * 1000,
    }


def _parse_authorization_input(value: str) -> Dict[str, str]:
    stripped = value.strip()
    if not stripped:
        return {}
    try:
        parsed = urllib.parse.urlparse(stripped)
        if parsed.scheme and parsed.netloc:
            params = urllib.parse.parse_qs(parsed.query)
            return {key: values[0] for key, values in params.items() if values}
    except ValueError:
        pass
    if "#" in stripped:
        code_value, state = stripped.split("#", 1)
        return {"code": code_value, "state": state}
    if "code=" in stripped:
        params = urllib.parse.parse_qs(stripped)
        return {key: values[0] for key, values in params.items() if values}
    return {"code": stripped}


def _generate_pkce() -> tuple[str, str]:
    verifier = _base64url(secrets.token_bytes(32))
    challenge = _base64url(hashlib.sha256(verifier.encode("ascii")).digest())
    return verifier, challenge


def _base64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


class _CallbackServer:
    def __init__(self, expected_state: str) -> None:
        self.expected_state = expected_state
        self.queue: "queue.Queue[Dict[str, str]]" = queue.Queue(maxsize=1)
        self.server: Optional[HTTPServer] = None
        self.thread: Optional[threading.Thread] = None

    def start(self) -> None:
        parent = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802
                parsed = urllib.parse.urlparse(self.path)
                params = urllib.parse.parse_qs(parsed.query)
                if parsed.path != CALLBACK_PATH:
                    self._reply(404, "Anthropic callback route not found.")
                    return
                if params.get("error"):
                    self._reply(400, f"Anthropic authentication failed: {params['error'][0]}")
                    return
                code = params.get("code", [None])[0]
                state = params.get("state", [None])[0]
                if not code or not state:
                    self._reply(400, "Missing code or state.")
                    return
                if state != parent.expected_state:
                    self._reply(400, "OAuth state mismatch.")
                    return
                with contextlib_suppress_queue_full(parent.queue):
                    parent.queue.put_nowait({"code": code, "state": state})
                self._reply(200, "Anthropic authentication completed. You can close this window.")

            def log_message(self, format: str, *args: Any) -> None:
                return

            def _reply(self, status: int, text: str) -> None:
                body = f"<html><body><p>{text}</p></body></html>".encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = HTTPServer((CALLBACK_HOST, CALLBACK_PORT), Handler)
        self.server.timeout = 0.5
        self.thread = threading.Thread(target=self._serve, name="anthropic-oauth-callback", daemon=True)
        self.thread.start()

    def wait(self, timeout_s: float) -> Optional[Dict[str, str]]:
        try:
            return self.queue.get(timeout=timeout_s)
        except queue.Empty:
            return None

    def stop(self) -> None:
        if self.server is not None:
            self.server.shutdown()
        if self.thread is not None:
            self.thread.join(timeout=1)
        if self.server is not None:
            self.server.server_close()

    def _serve(self) -> None:
        assert self.server is not None
        self.server.serve_forever(poll_interval=0.2)


class contextlib_suppress_queue_full:
    def __init__(self, q: "queue.Queue[Dict[str, str]]") -> None:
        self.q = q

    def __enter__(self) -> None:
        return None

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> bool:
        return exc_type is queue.Full
