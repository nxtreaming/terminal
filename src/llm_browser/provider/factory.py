from __future__ import annotations

import os
from typing import Any, Dict, Optional

from llm_browser.config import config_get
from llm_browser.llm.registry import get_model_spec
from llm_browser.provider.base import Provider


def make_provider(provider_name: str, model: Optional[str], config: Optional[Dict[str, Any]] = None) -> Optional[Provider]:
    provider = (provider_name or "fake").strip().lower()
    if provider == "fake":
        return None

    spec = get_model_spec(provider, model)
    if spec.transport == "codex_responses":
        from llm_browser.provider.codex_responses import CodexResponsesProvider

        return CodexResponsesProvider(model=spec.model)
    if spec.transport == "openai_responses":
        from llm_browser.provider.openai_responses import OpenAIResponsesProvider

        return OpenAIResponsesProvider(
            api_key=_api_key(spec.provider, config),
            model=spec.model,
            base_url=_base_url(spec.provider, config),
        )
    if spec.transport == "anthropic_messages":
        from llm_browser.provider.anthropic_messages import AnthropicMessagesProvider

        credential = _credential(spec.provider, config)
        return AnthropicMessagesProvider(
            api_key=credential.key if credential is not None else None,
            credential_type=credential.credential_type if credential is not None else "api_key",
            model=spec.model,
            base_url=_base_url(spec.provider, config) or "https://api.anthropic.com",
        )
    if spec.transport == "openai_compatible_chat":
        from llm_browser.provider.openai_compatible_chat import OpenAICompatibleChatProvider

        return OpenAICompatibleChatProvider(
            api_key=_api_key(spec.provider, config),
            model=spec.model,
            provider_label=spec.provider,
            base_url=_base_url(spec.provider, config) or spec.base_url or "",
            supports_images=spec.supports_images,
            extra_body=spec.extra_body,
        )
    raise ValueError(f"Unsupported transport for provider={provider}: {spec.transport}")


def _credential(provider: str, config: Optional[Dict[str, Any]]):
    from llm_browser.auth.store import ProviderAuthStore

    return ProviderAuthStore().resolve(provider, config_key=_config_api_key(provider, config))


def _api_key(provider: str, config: Optional[Dict[str, Any]]) -> Optional[str]:
    credential = _credential(provider, config)
    return credential.key if credential is not None else None


def _config_api_key(provider: str, config: Optional[Dict[str, Any]]) -> Optional[str]:
    if not config:
        return None
    value = config_get(config, f"providers.{provider}.api_key")
    if value:
        return str(value)
    if provider == "openai":
        value = config_get(config, "openai.api_key")
        return str(value) if value else None
    return None


def _base_url(provider: str, config: Optional[Dict[str, Any]]) -> Optional[str]:
    env_map = {
        "openai": "LLM_BROWSER_OPENAI_BASE_URL",
        "anthropic": "LLM_BROWSER_ANTHROPIC_BASE_URL",
        "zai": "LLM_BROWSER_ZAI_BASE_URL",
        "qwen": "LLM_BROWSER_QWEN_BASE_URL",
        "openrouter": "LLM_BROWSER_OPENROUTER_BASE_URL",
    }
    if not config:
        env_value = os.environ.get(env_map.get(provider, ""))
        return env_value.rstrip("/") if env_value else None
    value = config_get(config, f"providers.{provider}.base_url")
    if value:
        return str(value).rstrip("/")
    if provider == "openai":
        value = config_get(config, "openai.base_url")
        if value:
            return str(value).rstrip("/")
    env_value = os.environ.get(env_map.get(provider, ""))
    return env_value.rstrip("/") if env_value else None
