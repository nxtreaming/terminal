from __future__ import annotations

import json
import unittest
from pathlib import Path
from unittest.mock import patch

from llm_browser.auth import CodexAuth
from llm_browser.provider.codex_responses import CodexResponsesProvider, _codex_compact_url, _codex_url


class FakeResponse:
    def __init__(self, status_code: int, events: list, text: str = "", json_data=None) -> None:
        self.status_code = status_code
        self._events = events
        self.text = text
        self._json_data = json_data
        self.closed = False

    def iter_lines(self, decode_unicode: bool = False):
        for event in self._events:
            yield "data: " + __import__("json").dumps(event)
            yield ""

    def close(self) -> None:
        self.closed = True

    def json(self):
        if self._json_data is None:
            raise json.JSONDecodeError("no json", "", 0)
        return self._json_data


def fake_auth() -> CodexAuth:
    return CodexAuth(
        access_token="access-secret",
        account_id="acct_123",
        refresh_token="refresh-secret",
        id_token=None,
        source_path=Path("/tmp/auth.json"),
        auth_mode="chatgpt",
    )


class CodexResponsesProviderTest(unittest.TestCase):
    def test_codex_url_normalization(self) -> None:
        self.assertEqual(_codex_url("https://chatgpt.com/backend-api"), "https://chatgpt.com/backend-api/codex/responses")
        self.assertEqual(_codex_url("https://x/codex"), "https://x/codex/responses")
        self.assertEqual(_codex_url("https://x/codex/responses"), "https://x/codex/responses")
        self.assertEqual(_codex_compact_url("https://chatgpt.com/backend-api"), "https://chatgpt.com/backend-api/codex/responses/compact")

    def test_posts_with_codex_headers_and_parses_call(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test")
        response = FakeResponse(
            200,
            [
                {"type": "response.created", "response": {"id": "resp_1"}},
                {
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "python",
                        "arguments": "{\"code\":\"result = 1\"}",
                    },
                },
                {"type": "response.completed", "response": {"id": "resp_1", "output": []}},
            ],
        )

        with patch("llm_browser.provider.codex_responses.requests.post", return_value=response) as post:
            events = list(provider.start_turn([{"role": "user", "content": "open site"}], []))

        self.assertEqual(events[0].tool_call.name, "python")
        self.assertEqual(events[0].tool_call.arguments, {"code": "result = 1"})
        headers = post.call_args.kwargs["headers"]
        self.assertEqual(headers["Authorization"], "Bearer access-secret")
        self.assertEqual(headers["chatgpt-account-id"], "acct_123")
        self.assertEqual(headers["OpenAI-Beta"], "responses=experimental")
        self.assertEqual(headers["Accept"], "text/event-stream")
        payload = post.call_args.kwargs["json"]
        self.assertTrue(payload["stream"])
        self.assertFalse(payload["store"])
        self.assertEqual(payload["model"], "gpt-test")
        self.assertIn("CDP is the source of truth", payload["instructions"])
        self.assertIn("Helpers are convenience wrappers", payload["instructions"])
        self.assertIn("they are not top-level tools", payload["instructions"])
        self.assertIn("Do not discover the browser through raw DevTools URLs", payload["instructions"])
        self.assertTrue(post.call_args.kwargs["stream"])
        self.assertTrue(response.closed)

    def test_emits_normalized_usage_from_completed_response(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-5.5")
        response = FakeResponse(
            200,
            [
                {
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "output": [],
                        "usage": {
                            "input_tokens": 2000,
                            "input_tokens_details": {"cached_tokens": 400},
                            "output_tokens": 120,
                            "output_tokens_details": {"reasoning_tokens": 30},
                            "total_tokens": 2120,
                        },
                    },
                }
            ],
        )

        with patch("llm_browser.provider.codex_responses.requests.post", return_value=response):
            events = list(provider.start_turn([{"role": "user", "content": "open site"}], []))

        self.assertEqual(events[-1].type, "usage")
        self.assertEqual(events[-1].model, "gpt-5.5")
        self.assertEqual(events[-1].provider, "codex")
        self.assertEqual(events[-1].token_usage.input_tokens, 1600)
        self.assertEqual(events[-1].token_usage.cache_read_tokens, 400)
        self.assertEqual(events[-1].token_usage.output_tokens, 90)
        self.assertEqual(events[-1].token_usage.reasoning_tokens, 30)

    def test_uses_custom_instructions_when_set(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test", instructions="custom codex instructions")
        response = FakeResponse(200, [{"type": "response.completed", "response": {"id": "resp_2", "output": []}}])

        with patch("llm_browser.provider.codex_responses.requests.post", return_value=response) as post:
            list(provider.start_turn([{"role": "user", "content": "inspect repo"}], []))

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["instructions"], "custom codex instructions")

    def test_sends_full_function_call_history_for_tool_output(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test")
        response = FakeResponse(200, [{"type": "response.completed", "response": {"id": "resp_2", "output": []}}])

        with patch("llm_browser.provider.codex_responses.requests.post", return_value=response) as post:
            list(
                provider.start_turn(
                    [
                        {"role": "user", "content": "open site"},
                        {
                            "role": "assistant",
                            "tool_calls": [{"id": "call_1", "name": "python", "arguments": {"code": "result = 1"}}],
                        },
                        {"role": "tool", "tool_call_id": "call_1", "name": "python", "content": "ok"},
                    ],
                    [],
                )
            )

        payload = post.call_args.kwargs["json"]
        self.assertNotIn("previous_response_id", payload)
        self.assertEqual(payload["input"][0]["role"], "user")
        self.assertEqual(payload["input"][1]["type"], "function_call")
        self.assertEqual(payload["input"][1]["call_id"], "call_1")
        self.assertEqual(payload["input"][2], {"type": "function_call_output", "call_id": "call_1", "output": "ok"})

    def test_sends_screenshot_tool_output_as_visual_context(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test")
        response = FakeResponse(200, [{"type": "response.completed", "response": {"id": "resp_2", "output": []}}])
        content = [
            {"type": "input_text", "text": "data={'ok': True}\nimages=[loaded]"},
            {"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,abc"},
        ]

        with patch("llm_browser.provider.codex_responses.requests.post", return_value=response) as post:
            list(
                provider.start_turn(
                    [
                        {"role": "user", "content": "open site"},
                        {
                            "role": "assistant",
                            "tool_calls": [{"id": "call_1", "name": "python", "arguments": {"code": "screenshot()"}}],
                        },
                        {"role": "tool", "tool_call_id": "call_1", "name": "python", "content": content},
                    ],
                    [],
                )
            )

        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["input"][2]["type"], "function_call_output")
        self.assertIn("screenshot image", payload["input"][2]["output"])
        self.assertEqual(payload["input"][3]["role"], "user")
        self.assertEqual(payload["input"][3]["content"][1]["type"], "input_image")

    def test_prunes_older_visual_context_images_before_request(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test")
        response = FakeResponse(200, [{"type": "response.completed", "response": {"id": "resp_2", "output": []}}])
        messages = [{"role": "user", "content": "inspect"}]
        for index in range(4):
            messages.extend(
                [
                    {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": f"call_{index}",
                                "name": "python",
                                "arguments": {"code": "capture_screenshot()"},
                            }
                        ],
                    },
                    {
                        "role": "tool",
                        "tool_call_id": f"call_{index}",
                        "name": "python",
                        "content": [
                            {"type": "input_text", "text": f"images=[shot_{index}]"},
                            {
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": f"data:image/png;base64,img{index}",
                            },
                        ],
                    },
                ]
            )

        with patch("llm_browser.provider.codex_responses.requests.post", return_value=response) as post:
            list(provider.start_turn(messages, []))

        payload_json = json.dumps(post.call_args.kwargs["json"])
        image_urls = []
        for item in post.call_args.kwargs["json"]["input"]:
            for content_item in item.get("content") or []:
                if content_item.get("type") == "input_image":
                    image_urls.append(content_item["image_url"])

        self.assertEqual(image_urls, ["data:image/png;base64,img2", "data:image/png;base64,img3"])
        self.assertNotIn("img0", payload_json)
        self.assertNotIn("img1", payload_json)
        self.assertIn("older screenshot image omitted", payload_json)

    def test_remote_compaction_posts_compact_payload_and_converts_output(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test")
        response = FakeResponse(
            200,
            [],
            json_data={
                "output": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "kept user"}],
                    },
                    {"type": "compaction", "encrypted_content": "encrypted-summary"},
                ]
            },
        )

        with patch("llm_browser.provider.codex_responses.requests.post", return_value=response) as post:
            result = provider.compact_conversation_history([{"role": "user", "content": "hello"}], [])

        self.assertEqual(post.call_args.args[0], "https://chatgpt.com/backend-api/codex/responses/compact")
        payload = post.call_args.kwargs["json"]
        self.assertEqual(payload["input"][0]["role"], "user")
        self.assertNotIn("stream", payload)
        self.assertEqual(result.messages[0], {"role": "user", "content": "kept user"})
        self.assertEqual(result.messages[1]["role"], "provider_item")
        self.assertEqual(result.messages[1]["item"]["encrypted_content"], "encrypted-summary")
        self.assertTrue(response.closed)

    def test_orphan_tool_output_is_recovered_as_user_context(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test")
        response = FakeResponse(200, [{"type": "response.completed", "response": {"id": "resp_2", "output": []}}])

        with patch("llm_browser.provider.codex_responses.requests.post", return_value=response) as post:
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

    def test_refreshes_auth_once_after_unauthorized_response(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test")
        unauthorized = FakeResponse(401, [], text="expired")
        ok = FakeResponse(200, [{"type": "response.completed", "response": {"id": "resp_2", "output": []}}])
        refreshed = CodexAuth(
            access_token="new-access",
            account_id="acct_123",
            refresh_token="new-refresh",
            id_token=None,
            source_path=Path("/tmp/auth.json"),
            auth_mode="chatgpt",
        )

        with patch("llm_browser.provider.codex_responses.requests.post", side_effect=[unauthorized, ok]) as post, patch(
            "llm_browser.provider.codex_responses.refresh_codex_auth", return_value=refreshed
        ) as refresh:
            list(provider.start_turn([{"role": "user", "content": "open site"}], []))

        self.assertTrue(unauthorized.closed)
        refresh.assert_called_once()
        self.assertEqual(post.call_count, 2)
        self.assertEqual(post.call_args_list[1].kwargs["headers"]["Authorization"], "Bearer new-access")

    def test_retries_transient_stream_response_errors(self) -> None:
        provider = CodexResponsesProvider(auth=fake_auth(), model="gpt-test")
        transient = FakeResponse(503, [], text="busy")
        ok = FakeResponse(200, [{"type": "response.completed", "response": {"id": "resp_2", "output": []}}])

        with patch("llm_browser.provider.codex_responses.time.sleep") as sleep, patch(
            "llm_browser.provider.codex_responses.requests.post",
            side_effect=[transient, ok],
        ) as post:
            list(provider.start_turn([{"role": "user", "content": "open site"}], []))

        self.assertTrue(transient.closed)
        self.assertEqual(post.call_count, 2)
        sleep.assert_called_once()


if __name__ == "__main__":
    raise SystemExit(unittest.main())
