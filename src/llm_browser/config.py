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
        user_config_path(),
        root / ".browser-use-terminal" / DEFAULT_CONFIG_NAME,
    ]


def user_config_path() -> Path:
    return Path.home() / ".browser-use-terminal" / DEFAULT_CONFIG_NAME


def writable_config_path(explicit_path: Optional[Union[str, Path]] = None) -> Path:
    if explicit_path:
        return Path(explicit_path).expanduser()
    env_path = os.environ.get(ENV_CONFIG_PATH)
    if env_path:
        return Path(env_path).expanduser()
    return user_config_path()


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
    _write_config_file(target, example_config())
    return target


def write_config_value(
    dotted: str,
    value: Any,
    *,
    path: Optional[Union[str, Path]] = None,
) -> tuple[Path, Dict[str, Any]]:
    target = writable_config_path(path)
    config = _read_config_file(target)
    _deep_set(config, dotted, value)
    _write_config_file(target, config)
    return target, config


def write_config_values(
    values: Dict[str, Any],
    *,
    path: Optional[Union[str, Path]] = None,
) -> tuple[Path, Dict[str, Any]]:
    target = writable_config_path(path)
    config = _read_config_file(target)
    for dotted, value in values.items():
        _deep_set(config, dotted, value)
    _write_config_file(target, config)
    return target, config


def apply_config_environment(config: Dict[str, Any], *, override: bool = False) -> None:
    env_mappings = {
        "model": "LLM_BROWSER_MODEL",
        "openai.api_key": "LLM_BROWSER_OPENAI_API_KEY",
        "openai.base_url": "LLM_BROWSER_OPENAI_BASE_URL",
        "providers.openai.api_key": "LLM_BROWSER_OPENAI_API_KEY",
        "providers.openai.base_url": "LLM_BROWSER_OPENAI_BASE_URL",
        "providers.anthropic.api_key": "LLM_BROWSER_ANTHROPIC_API_KEY",
        "providers.anthropic.base_url": "LLM_BROWSER_ANTHROPIC_BASE_URL",
        "providers.zai.api_key": "LLM_BROWSER_ZAI_API_KEY",
        "providers.zai.base_url": "LLM_BROWSER_ZAI_BASE_URL",
        "providers.qwen.api_key": "LLM_BROWSER_QWEN_API_KEY",
        "providers.qwen.base_url": "LLM_BROWSER_QWEN_BASE_URL",
        "providers.openrouter.api_key": "LLM_BROWSER_OPENROUTER_API_KEY",
        "providers.openrouter.base_url": "LLM_BROWSER_OPENROUTER_BASE_URL",
        "codex.base_url": "LLM_BROWSER_CODEX_BASE_URL",
        "browser.cloud_api_key": "BROWSER_USE_API_KEY",
        "browser.cloud_api_base": "LLM_BROWSER_CLOUD_API_BASE",
        "browser.mode": "LLM_BROWSER_MODE",
        "browser.cdp_url": "LLM_BROWSER_CDP_HTTP_URL",
        "browser.cdp_ws": "LLM_BROWSER_CDP_WS_URL",
        "browser.chrome_path": "LLM_BROWSER_CHROME_PATH",
        "browser.profile_template": "LLM_BROWSER_PROFILE_TEMPLATE",
        "browser.width": "LLM_BROWSER_WIDTH",
        "browser.height": "LLM_BROWSER_HEIGHT",
        "browser.cloud_profile_id": "LLM_BROWSER_CLOUD_PROFILE_ID",
        "browser.cloud_profile_name": "LLM_BROWSER_CLOUD_PROFILE_NAME",
        "browser.cloud_proxy_country": "LLM_BROWSER_CLOUD_PROXY_COUNTRY",
        "browser.cloud_timeout": "LLM_BROWSER_CLOUD_TIMEOUT",
        "browser.cloud_custom_proxy_json": "LLM_BROWSER_CLOUD_CUSTOM_PROXY_JSON",
        "browser.daemon_name": "LLM_BROWSER_DAEMON_NAME",
        "browser.daemon_backend": "LLM_BROWSER_DAEMON_BACKEND",
    }
    bool_mappings = {
        "browser.headless": "LLM_BROWSER_HEADLESS",
        "browser.keep_profile": "LLM_BROWSER_KEEP_CHROME_PROFILE",
        "browser.cloud_allow_resizing": "LLM_BROWSER_CLOUD_ALLOW_RESIZING",
        "browser.cloud_recording": "LLM_BROWSER_CLOUD_ENABLE_RECORDING",
    }
    for dotted, env_name in env_mappings.items():
        value = config_get(config, dotted)
        if value is None or value == "":
            continue
        env_value = json.dumps(value) if isinstance(value, (dict, list)) else str(value)
        _set_env(env_name, env_value, override=override)
    for dotted, env_name in bool_mappings.items():
        value = config_get(config, dotted)
        if value is None:
            continue
        _set_env(env_name, "1" if _config_bool(value) else "0", override=override)
    provider = config_get(config, "provider")
    model = config_get(config, "model")
    if provider == "codex" and model:
        _set_env("LLM_BROWSER_CODEX_MODEL", str(model), override=override)


def example_config() -> Dict[str, Any]:
    return {
        "provider": "codex",
        "model": "gpt-5.5",
        "max_turns": 80,
        "browser": {
            "mode": "auto",
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
            "cloud_api_key": None,
            "cloud_api_base": None,
            "cloud_proxy_country": None,
            "cloud_timeout": None,
            "cloud_allow_resizing": None,
            "cloud_recording": None,
            "cloud_custom_proxy_json": None,
            "daemon_name": None,
            "daemon_backend": None,
        },
        "openai": {
            "api_key": None,
            "base_url": None,
        },
        "codex": {
            "base_url": None,
        },
        "providers": {
            "openai": {
                "api_key": None,
                "base_url": None,
            },
            "anthropic": {
                "api_key": None,
                "base_url": None,
            },
            "zai": {
                "api_key": None,
                "base_url": "https://api.z.ai/api/paas/v4",
            },
            "qwen": {
                "api_key": None,
                "base_url": "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            },
            "openrouter": {
                "api_key": None,
                "base_url": "https://openrouter.ai/api/v1",
            },
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
    _redact_secrets(redacted)
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


def _read_config_file(path: Path) -> Dict[str, Any]:
    if not path.exists():
        return {}
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError(f"config must be a JSON object: {path}")
    data.pop("_sources", None)
    return data


def _write_config_file(path: Path, config: Dict[str, Any]) -> None:
    clean = _deep_merge({}, config)
    clean.pop("_sources", None)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(clean, indent=2) + "\n", encoding="utf-8")
    try:
        path.chmod(0o600)
    except OSError:
        pass


def _deep_set(config: Dict[str, Any], dotted: str, value: Any) -> None:
    parts = [part for part in dotted.split(".") if part]
    if not parts:
        raise ValueError("config key cannot be empty")
    target: Dict[str, Any] = config
    for part in parts[:-1]:
        existing = target.get(part)
        if not isinstance(existing, dict):
            existing = {}
            target[part] = existing
        target = existing
    target[parts[-1]] = value


def _set_env(env_name: str, value: str, *, override: bool) -> None:
    if override or not os.environ.get(env_name):
        os.environ[env_name] = value


def _config_bool(value: Any) -> bool:
    if isinstance(value, str):
        return value.strip().lower() in {"1", "true", "yes", "on"}
    return bool(value)


def _redact_secrets(value: Any) -> None:
    if isinstance(value, dict):
        for item_key in list(value.keys()):
            normalized = item_key.lower().replace("-", "_")
            if any(marker in normalized for marker in ("api_key", "apikey", "token", "secret", "password")) and value[item_key]:
                value[item_key] = "<redacted>"
            else:
                _redact_secrets(value[item_key])
    elif isinstance(value, list):
        for item in value:
            _redact_secrets(item)
