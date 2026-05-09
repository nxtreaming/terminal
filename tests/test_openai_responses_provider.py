from __future__ import annotations

import unittest
from unittest.mock import patch

from llm_browser.provider.openai_responses import OpenAIResponsesProvider


class FakeResponse:
    def __init__(self, status_code: int, data: dict, text: str = "") -> None:
        self.status_code = status_code
        self._data = data
        self.text = text
        self.closed = False

    def json(self) -> dict:
        return self._data

    def close(self) -> None:
        self.closed = True


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
        self.assertIn("CDP is the source of truth", payload["instructions"])
        self.assertIn("Helpers are convenience wrappers", payload["instructions"])
        self.assertIn("they are not top-level tools", payload["instructions"])
        self.assertIn("Do not discover the browser through raw DevTools URLs", payload["instructions"])

    def test_emits_normalized_usage_event(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="gpt-5.5")
        response = FakeResponse(
            200,
            {
                "id": "resp_1",
                "output": [],
                "usage": {
                    "input_tokens": 1000,
                    "input_tokens_details": {"cached_tokens": 100},
                    "output_tokens": 70,
                    "output_tokens_details": {"reasoning_tokens": 20},
                    "total_tokens": 1070,
                },
            },
        )

        with patch("llm_browser.provider.openai_responses.requests.post", return_value=response):
            events = list(provider.start_turn([{"role": "user", "content": "hello"}], []))

        self.assertEqual(events[-1].type, "usage")
        self.assertEqual(events[-1].model, "gpt-5.5")
        self.assertEqual(events[-1].provider, "openai")
        self.assertEqual(events[-1].token_usage.input_tokens, 900)
        self.assertEqual(events[-1].token_usage.cache_read_tokens, 100)
        self.assertEqual(events[-1].token_usage.output_tokens, 50)
        self.assertEqual(events[-1].token_usage.reasoning_tokens, 20)

    def test_uses_custom_instructions_when_set(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="test-model", instructions="custom instructions")
        response = FakeResponse(200, {"id": "resp_1", "output": []})

        with patch("llm_browser.provider.openai_responses.requests.post", return_value=response) as post:
            list(provider.start_turn([{"role": "user", "content": "inspect repo"}], []))

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["instructions"], "custom instructions")

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

    def test_sends_full_history_tool_output_without_previous_response_id(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="test-model")
        response = FakeResponse(200, {"id": "resp_1", "output": []})
        content = [
            {"type": "input_text", "text": "images=[frame_1, frame_2]"},
            {"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,abc"},
        ]

        with patch("llm_browser.provider.openai_responses.requests.post", return_value=response) as post:
            list(
                provider.start_turn(
                    [
                        {"role": "user", "content": "validate images"},
                        {
                            "role": "assistant",
                            "tool_calls": [{"id": "call_1", "name": "image_probe", "arguments": {}}],
                        },
                        {"role": "tool", "tool_call_id": "call_1", "name": "image_probe", "content": content},
                        {"role": "user", "content": "answer"},
                    ],
                    [],
                )
            )

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["input"][1]["type"], "function_call")
        self.assertEqual(payload["input"][2]["type"], "function_call_output")
        self.assertEqual(payload["input"][3]["role"], "user")
        self.assertEqual(payload["input"][3]["content"][1]["type"], "input_image")
        self.assertEqual(payload["input"][4]["content"][0]["text"], "answer")

    def test_full_history_orphan_tool_output_is_recovered_as_user_context(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="test-model")
        response = FakeResponse(200, {"id": "resp_1", "output": []})

        with patch("llm_browser.provider.openai_responses.requests.post", return_value=response) as post:
            list(
                provider.start_turn(
                    [
                        {"role": "user", "content": "compacted summary"},
                        {"role": "tool", "tool_call_id": "call_missing", "name": "python", "content": "late output"},
                    ],
                    [],
                )
            )

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["input"][1]["role"], "user")
        self.assertIn("Recovered tool output", payload["input"][1]["content"][0]["text"])
        self.assertNotIn("function_call_output", [item.get("type") for item in payload["input"]])

    def test_reset_session_clears_response_chain(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="test-model")
        provider.previous_response_id = "resp_1"
        provider._sent_tool_call_ids.add("call_1")

        provider.reset_session()

        self.assertIsNone(provider.previous_response_id)
        self.assertEqual(provider._sent_tool_call_ids, set())

    def test_retries_transient_response_errors(self) -> None:
        provider = OpenAIResponsesProvider(api_key="test-key", model="test-model")
        transient = FakeResponse(500, {}, text="try again")
        ok = FakeResponse(200, {"id": "resp_1", "output": [{"type": "message", "content": [{"type": "output_text", "text": "done"}]}]})

        with patch("llm_browser.provider.openai_responses.time.sleep") as sleep, patch(
            "llm_browser.provider.openai_responses.requests.post",
            side_effect=[transient, ok],
        ) as post:
            events = list(provider.start_turn([{"role": "user", "content": "hello"}], []))

        self.assertTrue(transient.closed)
        self.assertEqual(post.call_count, 2)
        sleep.assert_called_once()
        self.assertEqual(events[0].text, "done")


if __name__ == "__main__":
    raise SystemExit(unittest.main())
