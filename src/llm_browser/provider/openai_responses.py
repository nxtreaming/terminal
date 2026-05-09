from __future__ import annotations

import json
import os
import time
from copy import deepcopy
from typing import Any, Dict, Iterable, List, Optional, Set

import requests

from llm_browser.browser.instructions import BROWSER_AGENT_INSTRUCTIONS
from llm_browser.provider.tool_content import tool_output_text, trim_visual_context_images, visual_context_messages
from llm_browser.provider.types import ModelEvent, ToolCall
from llm_browser.session.usage import ModelTokenUsage


class OpenAIResponsesProvider:
    """Small OpenAI Responses provider.

    This intentionally keeps transport state here rather than in the agent loop.
    The agent only knows about model deltas and tool calls.
    """

    def __init__(
        self,
        api_key: Optional[str] = None,
        model: Optional[str] = None,
        base_url: Optional[str] = None,
        timeout_s: float = 120.0,
        max_retries: int = 2,
        instructions: Optional[str] = None,
    ) -> None:
        self.api_key = api_key or os.environ.get("LLM_BROWSER_OPENAI_API_KEY") or os.environ.get("OPENAI_API_KEY")
        self.model = model or os.environ.get("LLM_BROWSER_MODEL") or "gpt-5.5"
        self.base_url = (base_url or os.environ.get("LLM_BROWSER_OPENAI_BASE_URL") or "https://api.openai.com/v1").rstrip(
            "/"
        )
        self.timeout_s = timeout_s
        self.max_retries = max(0, int(max_retries))
        self.previous_response_id: Optional[str] = None
        self._sent_tool_call_ids: Set[str] = set()
        self.provider_label = "openai"
        self.instructions = instructions or BROWSER_AGENT_INSTRUCTIONS

    def set_instructions(self, instructions: str) -> None:
        self.instructions = instructions

    def reset_session(self) -> None:
        self.previous_response_id = None
        self._sent_tool_call_ids.clear()

    def supports_remote_compaction(self) -> bool:
        return False

    def start_turn(
        self,
        messages: List[Dict[str, Any]],
        tools: List[Dict[str, Any]],
    ) -> Iterable[ModelEvent]:
        if not self.api_key:
            raise RuntimeError("OpenAI API key missing. Set LLM_BROWSER_OPENAI_API_KEY or OPENAI_API_KEY.")

        payload = self._build_payload(messages, tools)
        response = self._post_payload(payload)
        if response.status_code >= 400:
            raise RuntimeError(f"OpenAI Responses request failed: HTTP {response.status_code}: {response.text[:1000]}")

        data = response.json()
        self.previous_response_id = data.get("id") or self.previous_response_id
        for input_item in payload["input"]:
            if input_item.get("type") == "function_call_output":
                self._sent_tool_call_ids.add(str(input_item["call_id"]))

        for item in data.get("output") or []:
            item_type = item.get("type")
            if item_type == "message":
                yield from self._events_from_message(item)
            elif item_type == "function_call":
                yield self._event_from_function_call(item)
            elif item_type in {"reasoning", "web_search_call"}:
                continue
            else:
                text = self._extract_text_from_unknown_item(item)
                if text:
                    yield ModelEvent.text(text)
        usage = ModelTokenUsage.from_openai_usage(data.get("usage"))
        if usage is not None:
            yield ModelEvent.usage(usage, model=self.model, provider="openai")

    def _post_payload(self, payload: Dict[str, Any]) -> requests.Response:
        last_exc: Optional[BaseException] = None
        transient_statuses = {408, 409, 429, 500, 502, 503, 504}
        for attempt in range(self.max_retries + 1):
            try:
                response = requests.post(
                    f"{self.base_url}/responses",
                    headers={
                        "Authorization": f"Bearer {self.api_key}",
                        "Content-Type": "application/json",
                    },
                    json=payload,
                    timeout=self.timeout_s,
                )
                if response.status_code not in transient_statuses or attempt >= self.max_retries:
                    return response
                close = getattr(response, "close", None)
                if callable(close):
                    close()
            except requests.RequestException as exc:
                last_exc = exc
                if attempt >= self.max_retries:
                    raise
            time.sleep(min(2.0, 0.25 * (2**attempt)))
        if last_exc is not None:
            raise last_exc
        raise RuntimeError("OpenAI Responses request failed before receiving a response")

    def _build_payload(self, messages: List[Dict[str, Any]], tools: List[Dict[str, Any]]) -> Dict[str, Any]:
        input_items: List[Dict[str, Any]] = []

        unsent_tool_messages = [
            message
            for message in messages
            if message.get("role") == "tool" and str(message.get("tool_call_id")) not in self._sent_tool_call_ids
        ]
        if self.previous_response_id and unsent_tool_messages:
            for message in unsent_tool_messages:
                call_id = str(message["tool_call_id"])
                tool_name = str(message.get("name") or "tool")
                content = message.get("content", "")
                input_items.append(
                    {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": tool_output_text(content),
                    }
                )
                input_items.extend(visual_context_messages(content, call_id=call_id, tool_name=tool_name))
        elif any(message.get("role") in {"assistant", "tool"} for message in messages):
            input_items = self._convert_full_history(messages)
        else:
            user_text = self._latest_user_text(messages)
            input_items.append(
                {
                    "role": "user",
                    "content": [{"type": "input_text", "text": user_text}],
                }
            )

        input_items = trim_visual_context_images(input_items)

        payload: Dict[str, Any] = {
            "model": self.model,
            "input": input_items,
            "store": True,
            "instructions": self.instructions,
        }
        if tools:
            payload["tools"] = tools
            payload["parallel_tool_calls"] = True
        if self.previous_response_id:
            payload["previous_response_id"] = self.previous_response_id
        return payload

    def _convert_full_history(self, messages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        input_items: List[Dict[str, Any]] = []
        known_tool_call_ids: Set[str] = set()
        for message in messages:
            role = message.get("role")
            if role == "user":
                content = message.get("content", "")
                if isinstance(content, list):
                    input_items.append({"role": "user", "content": content})
                else:
                    input_items.append({"role": "user", "content": [{"type": "input_text", "text": str(content)}]})
            elif role == "assistant":
                text = message.get("content")
                if text:
                    input_items.append(
                        {
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": str(text)}],
                        }
                    )
                for call in message.get("tool_calls") or []:
                    call_id = str(call["id"])
                    known_tool_call_ids.add(call_id)
                    input_items.append(
                        {
                            "type": "function_call",
                            "call_id": call_id,
                            "name": str(call["name"]),
                            "arguments": json.dumps(call.get("arguments") or {}),
                        }
                    )
            elif role == "tool":
                call_id = str(message["tool_call_id"])
                tool_name = str(message.get("name") or "tool")
                content = message.get("content", "")
                if call_id not in known_tool_call_ids:
                    input_items.append(
                        {
                            "role": "user",
                            "content": [
                                {
                                    "type": "input_text",
                                    "text": (
                                        "Recovered tool output from compacted history. "
                                        f"The original function_call for {tool_name} ({call_id}) "
                                        "was outside the retained transcript.\n\n"
                                        f"{tool_output_text(content)}"
                                    ),
                                }
                            ],
                        }
                    )
                    input_items.extend(visual_context_messages(content, call_id=call_id, tool_name=tool_name))
                    continue
                input_items.append(
                    {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": tool_output_text(content),
                    }
                )
                input_items.extend(visual_context_messages(content, call_id=call_id, tool_name=tool_name))
            elif role == "provider_item":
                item = message.get("item")
                if isinstance(item, dict) and item.get("type") in {"compaction", "context_compaction"}:
                    input_items.append(deepcopy(item))
                else:
                    input_items.append(
                        {
                            "role": "user",
                            "content": [
                                {
                                    "type": "input_text",
                                    "text": "[provider-specific compacted context item retained from prior history]",
                                }
                            ],
                        }
                    )
        return input_items

    def _latest_user_text(self, messages: List[Dict[str, Any]]) -> str:
        for message in reversed(messages):
            if message.get("role") == "user":
                return str(message.get("content", ""))
        return ""

    def _events_from_message(self, item: Dict[str, Any]) -> Iterable[ModelEvent]:
        for content in item.get("content") or []:
            content_type = content.get("type")
            if content_type in {"output_text", "text"}:
                text = content.get("text")
                if text:
                    yield ModelEvent.text(str(text))

    def _event_from_function_call(self, item: Dict[str, Any]) -> ModelEvent:
        raw_arguments = item.get("arguments") or "{}"
        try:
            arguments = json.loads(raw_arguments)
        except json.JSONDecodeError:
            arguments = {"_raw": str(raw_arguments)}

        call_id = item.get("call_id") or item.get("id")
        if not call_id:
            raise RuntimeError(f"function_call item is missing call_id: {item}")

        return ModelEvent.call(
            ToolCall(
                id=str(call_id),
                name=str(item["name"]),
                arguments=arguments,
            )
        )

    def _extract_text_from_unknown_item(self, item: Dict[str, Any]) -> str:
        text = item.get("text")
        if isinstance(text, str):
            return text
        return ""
