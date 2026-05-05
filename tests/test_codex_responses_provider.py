from __future__ import annotations

import unittest
from pathlib import Path
from unittest.mock import patch

from llm_browser.auth import CodexAuth
from llm_browser.provider.codex_responses import CodexResponsesProvider, _codex_url


class FakeResponse:
    def __init__(self, status_code: int, events: list, text: str = "") -> None:
        self.status_code = status_code
        self._events = events
        self.text = text
        self.closed = False

    def iter_lines(self, decode_unicode: bool = False):
        for event in self._events:
            yield "data: " + __import__("json").dumps(event)
            yield ""

    def close(self) -> None:
        self.closed = True


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
        self.assertTrue(post.call_args.kwargs["stream"])
        self.assertTrue(response.closed)

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


if __name__ == "__main__":
    raise SystemExit(unittest.main())
