from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from llm_browser.auth.store import ProviderAuthStore
from llm_browser.llm.registry import default_model_for_provider, get_model_spec, provider_names
from llm_browser.provider.factory import make_provider


class ProviderFactoryTest(unittest.TestCase):
    def test_registry_defaults_include_new_providers(self) -> None:
        self.assertIn("anthropic", provider_names())
        self.assertIn("zai", provider_names())
        self.assertIn("qwen", provider_names())
        self.assertIn("openrouter", provider_names())
        self.assertEqual(default_model_for_provider("anthropic"), "claude-sonnet-4-6")
        self.assertEqual(default_model_for_provider("zai"), "glm-5.1")
        self.assertEqual(default_model_for_provider("qwen"), "qwen3.6-plus")
        self.assertEqual(default_model_for_provider("openrouter"), "z-ai/glm-5.1")

    def test_model_registry_selects_expected_transports(self) -> None:
        self.assertEqual(get_model_spec("openai", "gpt-5.5").transport, "openai_responses")
        self.assertEqual(get_model_spec("anthropic", None).transport, "anthropic_messages")
        self.assertEqual(get_model_spec("zai", None).transport, "openai_compatible_chat")
        self.assertEqual(get_model_spec("qwen", None).transport, "openai_compatible_chat")
        self.assertEqual(get_model_spec("openrouter", None).transport, "openai_compatible_chat")

    def test_model_registry_uses_provider_default_when_model_belongs_to_another_provider(self) -> None:
        self.assertEqual(get_model_spec("anthropic", "gpt-5.5").model, "claude-sonnet-4-6")
        self.assertEqual(get_model_spec("zai", "gpt-5.5").model, "glm-5.1")
        self.assertEqual(get_model_spec("qwen", "claude-sonnet-4-6").model, "qwen3.6-plus")

    def test_openrouter_short_aliases_expand_to_provider_slugs(self) -> None:
        self.assertEqual(get_model_spec("openrouter", "glm-5.1").model, "z-ai/glm-5.1")
        self.assertEqual(get_model_spec("openrouter", "glm-5").model, "z-ai/glm-5")
        self.assertEqual(get_model_spec("openrouter", "qwen3.6-plus").model, "qwen/qwen3.6-plus")
        self.assertEqual(get_model_spec("openrouter", "qwen3.6-flash").model, "qwen/qwen3.6-flash")

        custom = get_model_spec("openrouter", "qwen/custom-experimental")
        self.assertEqual(custom.model, "qwen/custom-experimental")
        self.assertEqual(custom.base_url, "https://openrouter.ai/api/v1")

    def test_factory_builds_zai_with_stored_api_key(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LLM_BROWSER_PROVIDER_AUTH_PATH": str(Path(tmp) / "auth.json")}, clear=True):
            ProviderAuthStore().set_api_key("zai", "zai-key")

            provider = make_provider("zai", None)

        self.assertIsNotNone(provider)
        self.assertEqual(getattr(provider, "model"), "glm-5.1")
        self.assertEqual(getattr(provider, "api_key"), "zai-key")

    def test_factory_builds_openrouter_with_stored_api_key(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LLM_BROWSER_PROVIDER_AUTH_PATH": str(Path(tmp) / "auth.json")}, clear=True):
            ProviderAuthStore().set_api_key("openrouter", "or-key")

            provider = make_provider("openrouter", "qwen3.6-plus")

        self.assertIsNotNone(provider)
        self.assertEqual(getattr(provider, "model"), "qwen/qwen3.6-plus")
        self.assertEqual(getattr(provider, "api_key"), "or-key")
        self.assertEqual(getattr(provider, "base_url"), "https://openrouter.ai/api/v1")
        self.assertTrue(getattr(provider, "supports_images"))


if __name__ == "__main__":
    raise SystemExit(unittest.main())
