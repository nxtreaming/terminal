from __future__ import annotations

import contextlib
import json
import os
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterator, Optional


AUTH_FILE_NAME = "provider-auth.json"

ENV_KEYS: dict[str, tuple[str, ...]] = {
    "openai": ("LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"),
    "anthropic": ("LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_OAUTH_TOKEN", "LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"),
    "zai": ("LLM_BROWSER_ZAI_API_KEY", "ZAI_API_KEY"),
    "qwen": ("LLM_BROWSER_QWEN_API_KEY", "QWEN_API_KEY", "DASHSCOPE_API_KEY"),
    "openrouter": ("LLM_BROWSER_OPENROUTER_API_KEY", "OPENROUTER_API_KEY"),
}


@dataclass(frozen=True)
class ResolvedCredential:
    key: str
    source: str
    credential_type: str = "api_key"


def default_provider_auth_path() -> Path:
    explicit = os.environ.get("LLM_BROWSER_PROVIDER_AUTH_PATH")
    if explicit:
        return Path(explicit).expanduser()
    home = os.environ.get("LLM_BROWSER_AUTH_HOME") or os.environ.get("BROWSER_USE_TERMINAL_AUTH_HOME")
    root = Path(home).expanduser() if home else Path.home() / ".browser-use-terminal"
    return root / AUTH_FILE_NAME


class ProviderAuthStore:
    def __init__(self, path: Optional[Path] = None) -> None:
        self.path = path or default_provider_auth_path()

    def get(self, provider: str) -> Optional[Dict[str, Any]]:
        return self._read().get(_normalize_provider(provider))

    def list(self) -> Dict[str, Dict[str, Any]]:
        return self._read()

    def set_api_key(self, provider: str, key: str) -> None:
        normalized = _normalize_provider(provider)
        data = self._read()
        data[normalized] = {"type": "api_key", "key": key}
        self._write(data)

    def set_oauth(self, provider: str, *, access: str, refresh: str, expires: int, extra: Optional[Dict[str, Any]] = None) -> None:
        normalized = _normalize_provider(provider)
        data = self._read()
        payload: Dict[str, Any] = {"type": "oauth", "access": access, "refresh": refresh, "expires": int(expires)}
        if extra:
            payload.update(extra)
        data[normalized] = payload
        self._write(data)

    def remove(self, provider: str) -> bool:
        normalized = _normalize_provider(provider)
        data = self._read()
        existed = normalized in data
        data.pop(normalized, None)
        self._write(data)
        return existed

    def status(self, provider: str) -> Dict[str, Any]:
        normalized = _normalize_provider(provider)
        credential = self.get(normalized)
        if credential:
            return {
                "available": True,
                "source": "stored",
                "type": credential.get("type") or "unknown",
                "path": str(self.path),
                "expires": credential.get("expires"),
            }
        env_name = first_env_key(normalized)
        if env_name:
            return {"available": True, "source": "environment", "env": env_name}
        return {"available": False}

    def resolve(self, provider: str, *, config_key: Optional[str] = None, refresh_oauth: bool = True) -> Optional[ResolvedCredential]:
        normalized = _normalize_provider(provider)
        if config_key:
            return ResolvedCredential(config_key, "config", "api_key")

        credential = self.get(normalized)
        if credential:
            ctype = str(credential.get("type") or "")
            if ctype in {"api_key", "api"}:
                key = credential.get("key")
                if key:
                    return ResolvedCredential(str(key), "stored", "api_key")
            if ctype == "oauth":
                access = credential.get("access")
                expires = int(credential.get("expires") or 0)
                if refresh_oauth and normalized == "anthropic" and expires and int(time.time() * 1000) >= expires:
                    return self._refresh_anthropic_oauth()
                if access:
                    return ResolvedCredential(str(access), "stored", "oauth")

        env_name = first_env_key(normalized)
        if env_name:
            credential_type = "oauth" if normalized == "anthropic" and "OAUTH" in env_name else "api_key"
            return ResolvedCredential(str(os.environ[env_name]), f"environment:{env_name}", credential_type)
        return None

    def _refresh_anthropic_oauth(self) -> Optional[ResolvedCredential]:
        from llm_browser.auth.anthropic import refresh_anthropic_token

        with _file_lock(self.path):
            data = self._read_unlocked()
            credential = data.get("anthropic")
            if not credential or credential.get("type") != "oauth":
                return None
            expires = int(credential.get("expires") or 0)
            if expires and int(time.time() * 1000) < expires:
                access = credential.get("access")
                return ResolvedCredential(str(access), "stored", "oauth") if access else None
            refreshed = refresh_anthropic_token(str(credential.get("refresh") or ""))
            data["anthropic"] = {"type": "oauth", **refreshed}
            self._write_unlocked(data)
            return ResolvedCredential(str(refreshed["access"]), "stored", "oauth")

    def _read(self) -> Dict[str, Dict[str, Any]]:
        with _file_lock(self.path):
            return self._read_unlocked()

    def _write(self, data: Dict[str, Dict[str, Any]]) -> None:
        with _file_lock(self.path):
            self._write_unlocked(data)

    def _read_unlocked(self) -> Dict[str, Dict[str, Any]]:
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return {}
        except json.JSONDecodeError:
            return {}
        if not isinstance(raw, dict):
            return {}
        return {str(k): v for k, v in raw.items() if isinstance(v, dict)}

    def _write_unlocked(self, data: Dict[str, Dict[str, Any]]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        try:
            tmp.chmod(0o600)
        except OSError:
            pass
        os.replace(tmp, self.path)
        try:
            self.path.chmod(0o600)
        except OSError:
            pass


def provider_auth_status() -> Dict[str, Any]:
    store = ProviderAuthStore()
    return {provider: store.status(provider) for provider in ENV_KEYS}


def resolve_provider_key(provider: str, *, config_key: Optional[str] = None) -> Optional[ResolvedCredential]:
    return ProviderAuthStore().resolve(provider, config_key=config_key)


def first_env_key(provider: str) -> Optional[str]:
    for key in ENV_KEYS.get(_normalize_provider(provider), ()):
        if os.environ.get(key):
            return key
    return None


def _normalize_provider(provider: str) -> str:
    return provider.strip().lower().rstrip("/")


@contextlib.contextmanager
def _file_lock(path: Path) -> Iterator[None]:
    lock_path = path.with_suffix(path.suffix + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    deadline = time.monotonic() + 10
    fd: Optional[int] = None
    while fd is None:
        try:
            fd = os.open(str(lock_path), os.O_CREAT | os.O_EXCL | os.O_RDWR)
        except FileExistsError:
            if time.monotonic() > deadline:
                raise TimeoutError(f"timed out waiting for auth lock: {lock_path}")
            time.sleep(0.05)
    try:
        yield
    finally:
        try:
            os.close(fd)
        finally:
            with contextlib.suppress(FileNotFoundError):
                lock_path.unlink()
