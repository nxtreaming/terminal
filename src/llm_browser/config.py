from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Dict, Optional, Union


DEFAULT_CONFIG_NAME = "config.json"
ENV_CONFIG_PATH = "BROWSER_USE_TERMINAL_CONFIG"


def default_config_paths(cwd: Optional[Path] = None) -> list[Path]:
    root = cwd or Path.cwd()
    return [
        Path.home() / ".browser-use-terminal" / DEFAULT_CONFIG_NAME,
        root / ".browser-use-terminal" / DEFAULT_CONFIG_NAME,
    ]


def load_config(explicit_path: Optional[Union[str, Path]] = None, cwd: Optional[Path] = None) -> Dict[str, Any]:
    paths = _config_paths(explicit_path=explicit_path, cwd=cwd)
    merged: Dict[str, Any] = {}
    sources = []
    for path in paths:
        if not path.exists():
            continue
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            raise ValueError(f"config must be a JSON object: {path}")
        merged = _deep_merge(merged, data)
        sources.append(str(path))
    if sources:
        merged["_sources"] = sources
    return merged


def write_default_config(path: Union[str, Path], *, force: bool = False) -> Path:
    target = Path(path).expanduser()
    if target.exists() and not force:
        raise FileExistsError(f"config already exists: {target}")
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(example_config(), indent=2) + "\n", encoding="utf-8")
    return target


def example_config() -> Dict[str, Any]:
    return {
        "provider": "codex",
        "model": "gpt-5.5",
        "max_turns": 80,
        "browser": {
            "mode": "chromium",
            "headless": False,
            "width": 1280,
            "height": 900,
            "chrome_path": None,
            "profile_template": None,
            "keep_profile": False,
            "cdp_url": None,
            "cdp_ws": None,
            "cloud_profile_id": None,
            "cloud_profile_name": None,
            "cloud_proxy_country": None,
            "cloud_timeout": None,
            "cloud_allow_resizing": None,
            "cloud_recording": None,
            "cloud_custom_proxy_json": None,
            "daemon_name": None,
            "daemon_backend": None,
        },
    }


def config_get(config: Dict[str, Any], dotted: str, default: Any = None) -> Any:
    value: Any = config
    for part in dotted.split("."):
        if not isinstance(value, dict) or part not in value:
            return default
        value = value[part]
    return value


def redacted_config(config: Dict[str, Any]) -> Dict[str, Any]:
    redacted = _deep_merge({}, config)
    for key in ("api_key", "token", "access_token", "refresh_token"):
        _redact_key(redacted, key)
    return redacted


def _config_paths(explicit_path: Optional[Union[str, Path]], cwd: Optional[Path]) -> list[Path]:
    if explicit_path:
        return [Path(explicit_path).expanduser()]
    env_path = os.environ.get(ENV_CONFIG_PATH)
    if env_path:
        return [Path(env_path).expanduser()]
    return default_config_paths(cwd=cwd)


def _deep_merge(base: Dict[str, Any], override: Dict[str, Any]) -> Dict[str, Any]:
    merged = dict(base)
    for key, value in override.items():
        existing = merged.get(key)
        if isinstance(existing, dict) and isinstance(value, dict):
            merged[key] = _deep_merge(existing, value)
        else:
            merged[key] = value
    return merged


def _redact_key(value: Any, key: str) -> None:
    if isinstance(value, dict):
        for item_key in list(value.keys()):
            if item_key.lower() == key and value[item_key]:
                value[item_key] = "<redacted>"
            else:
                _redact_key(value[item_key], key)
    elif isinstance(value, list):
        for item in value:
            _redact_key(item, key)
