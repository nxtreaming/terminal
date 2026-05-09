from __future__ import annotations

import unittest
from unittest.mock import patch

from llm_browser.provider.openai_compatible_chat import OpenAICompatibleChatProvider


class FakeResponse:
    def __init__(self, status_code: int, data: dict, text: str = "") -> None:
        self.status_code = status_code
        self._data = data
        self.text = text

    def json(self) -> dict:
        return self._data


class OpenAICompatibleChatProviderTest(unittest.TestCase):
    def test_builds_chat_payload_and_parses_tool_call(self) -> None:
        provider = OpenAICompatibleChatProvider(
            api_key="test-key",
            model="glm-5.1",
            provider_label="zai",
            base_url="https://api.z.ai/api/paas/v4",
            extra_body={"thinking": {"type": "enabled", "clear_thinking": False}},
        )
        response = FakeResponse(
            200,
            {
                "choices": [
                    {
                        "message": {
                            "content": "using tool",
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {"name": "echo", "arguments": "{\"text\":\"hello\"}"},
                                }
                            ],
                        }
                    }
                ],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            },
        )

        with patch("llm_browser.provider.openai_compatible_chat.requests.post", return_value=response) as post:
            events = list(
                provider.start_turn(
                    [{"role": "user", "content": "hello"}],
                    [{"type": "function", "name": "echo", "description": "", "parameters": {"type": "object"}}],
                )
            )

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["model"], "glm-5.1")
        self.assertEqual(payload["messages"][0]["role"], "system")
        self.assertEqual(payload["tools"][0]["function"]["name"], "echo")
        self.assertEqual(payload["thinking"]["type"], "enabled")
        self.assertEqual(events[0].text, "using tool")
        self.assertEqual(events[1].tool_call.name, "echo")
        self.assertEqual(events[1].tool_call.arguments, {"text": "hello"})
        self.assertEqual(events[-1].type, "usage")

    def test_text_only_provider_converts_images_to_markers(self) -> None:
        provider = OpenAICompatibleChatProvider(
            api_key="test-key",
            model="qwen3.6-plus",
            provider_label="qwen",
            base_url="https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            supports_images=False,
        )
        response = FakeResponse(200, {"choices": [{"message": {"content": "ok"}}]})
        content = [
            {"type": "input_text", "text": "frame"},
            {"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,abc"},
        ]

        with patch("llm_browser.provider.openai_compatible_chat.requests.post", return_value=response) as post:
            list(provider.start_turn([{"role": "user", "content": content}], []))

        user_content = post.call_args.kwargs["json"]["messages"][1]["content"]
        self.assertIn("frame", user_content)
        self.assertIn("Image attached", user_content)
        self.assertNotIn("base64,abc", user_content)

    def test_openrouter_payload_keeps_reasoning_config_and_images(self) -> None:
        provider = OpenAICompatibleChatProvider(
            api_key="test-key",
            model="qwen/qwen3.6-plus",
            provider_label="openrouter",
            base_url="https://openrouter.ai/api/v1",
            supports_images=True,
            extra_body={"reasoning": {"enabled": True, "exclude": True}},
        )
        response = FakeResponse(200, {"choices": [{"message": {"content": "ok"}}]})
        content = [
            {"type": "input_text", "text": "frame"},
            {"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,abc"},
        ]

        with patch("llm_browser.provider.openai_compatible_chat.requests.post", return_value=response) as post:
            list(provider.start_turn([{"role": "user", "content": content}], []))

        self.assertEqual(post.call_args.args[0], "https://openrouter.ai/api/v1/chat/completions")
        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["model"], "qwen/qwen3.6-plus")
        self.assertEqual(payload["reasoning"], {"enabled": True, "exclude": True})
        self.assertEqual(payload["messages"][1]["content"][1]["type"], "image_url")
        self.assertEqual(payload["messages"][1]["content"][1]["image_url"]["url"], "data:image/png;base64,abc")


if __name__ == "__main__":
    raise SystemExit(unittest.main())
