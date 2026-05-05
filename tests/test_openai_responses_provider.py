from __future__ import annotations

import unittest
from unittest.mock import patch

from llm_browser.provider.openai_responses import OpenAIResponsesProvider


class FakeResponse:
    def __init__(self, status_code: int, data: dict, text: str = "") -> None:
        self.status_code = status_code
        self._data = data
        self.text = text

    def json(self) -> dict:
        return self._data


class OpenAIResponsesProviderTest(unittest.TestCase):
    def test_parses_text_and_function_call(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="test-model")
        response = FakeResponse(
            200,
            {
                "id": "resp_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{"type": "output_text", "text": "Thinking aloud."}],
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "echo",
                        "arguments": "{\"text\":\"hello\"}",
                    },
                ],
            },
        )

        with patch("llm_browser.provider.openai_responses.requests.post", return_value=response) as post:
            events = list(
                provider.start_turn(
                    [{"role": "user", "content": "hello"}],
                    [{"type": "function", "name": "echo", "description": "", "parameters": {"type": "object"}}],
                )
            )

        self.assertEqual(events[0].type, "text_delta")
        self.assertEqual(events[0].text, "Thinking aloud.")
        self.assertEqual(events[1].type, "tool_call")
        self.assertEqual(events[1].tool_call.id, "call_1")
        self.assertEqual(events[1].tool_call.arguments, {"text": "hello"})

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["model"], "test-model")
        self.assertEqual(payload["input"][0]["role"], "user")
        self.assertEqual(payload["tools"][0]["name"], "echo")
        self.assertTrue(payload["store"])

    def test_sends_function_call_output_with_previous_response_id(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="test-model")
        provider.previous_response_id = "resp_1"
        response = FakeResponse(
            200,
            {
                "id": "resp_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{"type": "output_text", "text": "done"}],
                    }
                ],
            },
        )

        with patch("llm_browser.provider.openai_responses.requests.post", return_value=response) as post:
            events = list(
                provider.start_turn(
                    [
                        {"role": "user", "content": "hello"},
                        {"role": "tool", "tool_call_id": "call_1", "name": "echo", "content": "hello"},
                    ],
                    [],
                )
            )

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["previous_response_id"], "resp_1")
        self.assertEqual(
            payload["input"],
            [{"type": "function_call_output", "call_id": "call_1", "output": "hello"}],
        )
        self.assertEqual(events[0].text, "done")

    def test_sends_visual_context_after_screenshot_tool_output(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="test-model")
        provider.previous_response_id = "resp_1"
        response = FakeResponse(200, {"id": "resp_2", "output": []})
        content = [
            {"type": "input_text", "text": "images=[after-click]"},
            {"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,abc"},
        ]

        with patch("llm_browser.provider.openai_responses.requests.post", return_value=response) as post:
            list(
                provider.start_turn(
                    [
                        {"role": "user", "content": "hello"},
                        {"role": "tool", "tool_call_id": "call_1", "name": "python", "content": content},
                    ],
                    [],
                )
            )

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["input"][0]["type"], "function_call_output")
        self.assertIn("screenshot image", payload["input"][0]["output"])
        self.assertEqual(payload["input"][1]["role"], "user")
        self.assertEqual(payload["input"][1]["content"][1]["type"], "input_image")


if __name__ == "__main__":
    raise SystemExit(unittest.main())
