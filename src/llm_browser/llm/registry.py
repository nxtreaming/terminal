from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, Iterable, Optional


@dataclass(frozen=True)
class ModelSpec:
    provider: str
    model: str
    transport: str
    display_name: str
    description: str
    default: bool = False
    base_url: Optional[str] = None
    supports_images: bool = True
    extra_body: Dict[str, Any] = field(default_factory=dict)


_MODELS: tuple[ModelSpec, ...] = (
    ModelSpec("codex", "gpt-5.5", "codex_responses", "GPT-5.5", "Codex subscription default", default=True),
    ModelSpec("codex", "gpt-5.4", "codex_responses", "GPT-5.4", "Codex subscription fallback"),
    ModelSpec("openai", "gpt-5.5", "openai_responses", "GPT-5.5", "OpenAI Responses default", default=True),
    ModelSpec("openai", "gpt-5.5-pro", "openai_responses", "GPT-5.5 Pro", "OpenAI highest capability"),
    ModelSpec("openai", "gpt-5.4", "openai_responses", "GPT-5.4", "OpenAI everyday model"),
    ModelSpec("openai", "gpt-5.4-mini", "openai_responses", "GPT-5.4 Mini", "OpenAI fast model"),
    ModelSpec("openai", "gpt-5.4-nano", "openai_responses", "GPT-5.4 Nano", "OpenAI cheapest model"),
    ModelSpec(
        "anthropic",
        "claude-sonnet-4-6",
        "anthropic_messages",
        "Claude Sonnet 4.6",
        "Practical Claude default",
        default=True,
    ),
    ModelSpec(
        "anthropic",
        "claude-opus-4-7",
        "anthropic_messages",
        "Claude Opus 4.7",
        "Highest Claude capability",
    ),
    ModelSpec(
        "anthropic",
        "claude-haiku-4-5-20251001",
        "anthropic_messages",
        "Claude Haiku 4.5",
        "Fast Claude model",
    ),
    ModelSpec(
        "zai",
        "glm-5.1",
        "openai_compatible_chat",
        "GLM-5.1",
        "Z.ai latest GLM coding model",
        default=True,
        base_url="https://api.z.ai/api/paas/v4",
        supports_images=False,
        extra_body={"thinking": {"type": "enabled", "clear_thinking": False}},
    ),
    ModelSpec(
        "zai",
        "glm-5",
        "openai_compatible_chat",
        "GLM-5",
        "Z.ai GLM fallback",
        base_url="https://api.z.ai/api/paas/v4",
        supports_images=False,
        extra_body={"thinking": {"type": "enabled", "clear_thinking": False}},
    ),
    ModelSpec(
        "zai",
        "glm-4.7",
        "openai_compatible_chat",
        "GLM-4.7",
        "Z.ai GLM legacy fallback",
        base_url="https://api.z.ai/api/paas/v4",
        supports_images=False,
        extra_body={"thinking": {"type": "enabled", "clear_thinking": False}},
    ),
    ModelSpec(
        "qwen",
        "qwen3.6-plus",
        "openai_compatible_chat",
        "Qwen 3.6 Plus",
        "Qwen stable default",
        default=True,
        base_url="https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        supports_images=False,
        extra_body={"enable_thinking": True},
    ),
    ModelSpec(
        "qwen",
        "qwen3.6-plus-2026-04-02",
        "openai_compatible_chat",
        "Qwen 3.6 Plus 2026-04-02",
        "Pinned Qwen Plus build",
        base_url="https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        supports_images=False,
        extra_body={"enable_thinking": True},
    ),
    ModelSpec(
        "qwen",
        "qwen3.6-35b-a3b",
        "openai_compatible_chat",
        "Qwen 3.6 35B A3B",
        "Qwen smaller reasoning model",
        base_url="https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        supports_images=False,
        extra_body={"enable_thinking": True},
    ),
    ModelSpec(
        "qwen",
        "qwen3.6-27b",
        "openai_compatible_chat",
        "Qwen 3.6 27B",
        "Qwen compact model",
        base_url="https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        supports_images=False,
        extra_body={"enable_thinking": True},
    ),
    ModelSpec(
        "qwen",
        "qwen3.6-max-preview",
        "openai_compatible_chat",
        "Qwen 3.6 Max Preview",
        "Experimental Qwen max preview",
        base_url="https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        supports_images=False,
        extra_body={"enable_thinking": True},
    ),
    ModelSpec(
        "openrouter",
        "z-ai/glm-5.1",
        "openai_compatible_chat",
        "GLM-5.1",
        "OpenRouter Z.ai latest GLM coding model",
        default=True,
        base_url="https://openrouter.ai/api/v1",
        supports_images=False,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
    ModelSpec(
        "openrouter",
        "z-ai/glm-5",
        "openai_compatible_chat",
        "GLM-5",
        "OpenRouter Z.ai GLM flagship fallback",
        base_url="https://openrouter.ai/api/v1",
        supports_images=False,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
    ModelSpec(
        "openrouter",
        "z-ai/glm-5-turbo",
        "openai_compatible_chat",
        "GLM-5 Turbo",
        "OpenRouter Z.ai faster GLM fallback",
        base_url="https://openrouter.ai/api/v1",
        supports_images=False,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
    ModelSpec(
        "openrouter",
        "z-ai/glm-5v-turbo",
        "openai_compatible_chat",
        "GLM-5V Turbo",
        "OpenRouter Z.ai vision-capable GLM model",
        base_url="https://openrouter.ai/api/v1",
        supports_images=True,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
    ModelSpec(
        "openrouter",
        "qwen/qwen3.6-plus",
        "openai_compatible_chat",
        "Qwen 3.6 Plus",
        "OpenRouter Qwen high-capability agentic model",
        base_url="https://openrouter.ai/api/v1",
        supports_images=True,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
    ModelSpec(
        "openrouter",
        "qwen/qwen3.6-flash",
        "openai_compatible_chat",
        "Qwen 3.6 Flash",
        "OpenRouter Qwen fast 1M-context model",
        base_url="https://openrouter.ai/api/v1",
        supports_images=True,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
    ModelSpec(
        "openrouter",
        "qwen/qwen3.6-35b-a3b",
        "openai_compatible_chat",
        "Qwen 3.6 35B A3B",
        "OpenRouter Qwen compact open-weight model",
        base_url="https://openrouter.ai/api/v1",
        supports_images=True,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
    ModelSpec(
        "openrouter",
        "qwen/qwen3.6-27b",
        "openai_compatible_chat",
        "Qwen 3.6 27B",
        "OpenRouter Qwen compact model",
        base_url="https://openrouter.ai/api/v1",
        supports_images=True,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
    ModelSpec(
        "openrouter",
        "qwen/qwen3.6-max-preview",
        "openai_compatible_chat",
        "Qwen 3.6 Max Preview",
        "OpenRouter experimental Qwen max preview",
        base_url="https://openrouter.ai/api/v1",
        supports_images=False,
        extra_body={"reasoning": {"enabled": True, "exclude": True}},
    ),
)

_PROVIDER_LABELS = {
    "fake": ("Fake", "Use deterministic local fake provider"),
    "codex": ("Codex", "Use Codex subscription auth"),
    "openai": ("OpenAI", "Use OpenAI API key"),
    "anthropic": ("Anthropic", "Use Claude API key or Claude Code login"),
    "zai": ("Z.ai", "Use Z.ai GLM models"),
    "qwen": ("Qwen", "Use Qwen OpenAI-compatible API"),
    "openrouter": ("OpenRouter", "Use OpenRouter-hosted GLM and Qwen models"),
}


def provider_names() -> list[str]:
    return list(_PROVIDER_LABELS)


def provider_palette() -> list[tuple[str, str, str]]:
    rows = []
    for provider, (label, description) in _PROVIDER_LABELS.items():
        default_model = default_model_for_provider(provider)
        detail = f"{description}; default {default_model}" if default_model else description
        rows.append((label, f"provider {provider}", detail))
    return rows


def model_palette() -> list[tuple[str, str, str]]:
    return [(spec.display_name, f"model {spec.model}", f"{spec.provider}: {spec.description}") for spec in _MODELS]


def default_model_for_provider(provider: str) -> Optional[str]:
    normalized = provider.strip().lower()
    for spec in _MODELS:
        if spec.provider == normalized and spec.default:
            return spec.model
    for spec in _MODELS:
        if spec.provider == normalized:
            return spec.model
    return None


def get_model_spec(provider: str, model: Optional[str]) -> ModelSpec:
    normalized_provider = provider.strip().lower()
    requested_model = (model or default_model_for_provider(normalized_provider) or "").strip()
    if not requested_model:
        raise ValueError(f"No default model configured for provider={provider}")
    if "/" in requested_model and normalized_provider == "auto":
        maybe_provider, maybe_model = requested_model.split("/", 1)
        normalized_provider = maybe_provider.strip().lower()
        requested_model = maybe_model.strip()
    if normalized_provider == "auto":
        normalized_provider = infer_provider_from_model(requested_model)
    if normalized_provider == "openrouter":
        requested_model = _openrouter_model_alias(requested_model)

    exact = _find_model_spec(normalized_provider, requested_model)
    if exact is not None:
        return exact

    prefix_provider = _provider_from_known_model_prefix(requested_model)
    if normalized_provider != "openrouter" and prefix_provider is not None and prefix_provider != normalized_provider:
        default_model = default_model_for_provider(normalized_provider)
        if default_model:
            requested_model = default_model
            exact = _find_model_spec(normalized_provider, requested_model)
            if exact is not None:
                return exact

    inferred_transport = _transport_for_provider(normalized_provider)
    if inferred_transport is None:
        raise ValueError(f"Unknown provider: {provider}")
    return ModelSpec(
        provider=normalized_provider,
        model=requested_model,
        transport=inferred_transport,
        display_name=requested_model,
        description="Custom model",
        base_url=_default_base_url(normalized_provider),
        supports_images=normalized_provider in {"openai", "anthropic"},
        extra_body=_default_extra_body(normalized_provider),
    )


def infer_provider_from_model(model: str) -> str:
    provider = _provider_from_known_model_prefix(model)
    if provider is not None:
        return provider
    return "codex"


def _provider_from_known_model_prefix(model: str) -> Optional[str]:
    value = model.strip().lower()
    if value.startswith("claude-"):
        return "anthropic"
    if value.startswith("glm-"):
        return "zai"
    if value.startswith("qwen"):
        return "qwen"
    if value.startswith("gpt-"):
        return "openai"
    return None


def _openrouter_model_alias(model: str) -> str:
    value = model.strip()
    aliases = {
        "glm-5.1": "z-ai/glm-5.1",
        "glm-5": "z-ai/glm-5",
        "glm-5-turbo": "z-ai/glm-5-turbo",
        "glm-5v-turbo": "z-ai/glm-5v-turbo",
        "qwen3.6-plus": "qwen/qwen3.6-plus",
        "qwen3.6-flash": "qwen/qwen3.6-flash",
        "qwen3.6-35b-a3b": "qwen/qwen3.6-35b-a3b",
        "qwen3.6-27b": "qwen/qwen3.6-27b",
        "qwen3.6-max-preview": "qwen/qwen3.6-max-preview",
    }
    return aliases.get(value.lower(), value)


def _find_model_spec(provider: str, model: str) -> Optional[ModelSpec]:
    for spec in _MODELS:
        if spec.provider == provider and spec.model == model:
            return spec
    return None


def models_for_provider(provider: str) -> Iterable[ModelSpec]:
    normalized = provider.strip().lower()
    return (spec for spec in _MODELS if spec.provider == normalized)


def _transport_for_provider(provider: str) -> Optional[str]:
    if provider == "codex":
        return "codex_responses"
    if provider == "openai":
        return "openai_responses"
    if provider == "anthropic":
        return "anthropic_messages"
    if provider in {"zai", "qwen", "openrouter"}:
        return "openai_compatible_chat"
    if provider == "fake":
        return "fake"
    return None


def _default_base_url(provider: str) -> Optional[str]:
    if provider == "zai":
        return "https://api.z.ai/api/paas/v4"
    if provider == "qwen":
        return "https://dashscope-intl.aliyuncs.com/compatible-mode/v1"
    if provider == "openrouter":
        return "https://openrouter.ai/api/v1"
    return None


def _default_extra_body(provider: str) -> Dict[str, Any]:
    if provider == "zai":
        return {"thinking": {"type": "enabled", "clear_thinking": False}}
    if provider == "qwen":
        return {"enable_thinking": True}
    if provider == "openrouter":
        return {"reasoning": {"enabled": True, "exclude": True}}
    return {}
