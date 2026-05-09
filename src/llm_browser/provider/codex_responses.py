from __future__ import annotations

import json
import os
import time
from copy import deepcopy
from typing import Any, Dict, Iterable, List, Optional, Set

import requests

from llm_browser.auth import CodexAuth, CodexAuthError, PermanentCodexAuthError, load_codex_auth, refresh_codex_auth
from llm_browser.browser.instructions import BROWSER_AGENT_INSTRUCTIONS
from llm_browser.provider.tool_content import tool_output_text, trim_visual_context_images, visual_context_messages
from llm_browser.provider.types import ModelEvent, ProviderCompactionResult, ToolCall
from llm_browser.session.usage import ModelTokenUsage


DEFAULT_CODEX_BASE_URL = "https://chatgpt.com/backend-api"


class CodexResponsesProvider:
    """OpenAI Codex subscription backend provider.

    This uses an existing Codex CLI login from CODEX_HOME or ~/.codex. It does
    not write or refresh credentials yet; refresh is a later hardening slice.
    """

    def __init__(
        self,
        auth: Optional[CodexAuth] = None,
        model: Optional[str] = None,
        base_url: Optional[str] = None,
        timeout_s: float = 120.0,
        max_retries: int = 2,
        instructions: Optional[str] = None,
    ) -> None:
        self.auth = auth or load_codex_auth()
        self.model = model or os.environ.get("LLM_BROWSER_CODEX_MODEL") or os.environ.get("LLM_BROWSER_MODEL") or "gpt-5.5"
        self.base_url = base_url or os.environ.get("LLM_BROWSER_CODEX_BASE_URL") or DEFAULT_CODEX_BASE_URL
        self.timeout_s = timeout_s
        self.max_retries = max(0, int(max_retries))
        self.provider_label = "codex"
        self.instructions = instructions or BROWSER_AGENT_INSTRUCTIONS

    def set_instructions(self, instructions: str) -> None:
        self.instructions = instructions

    def reset_session(self) -> None:
        return None

    def supports_remote_compaction(self) -> bool:
        return True

    def compact_conversation_history(
        self,
        messages: List[Dict[str, Any]],
        tools: List[Dict[str, Any]],
    ) -> ProviderCompactionResult:
        if self.auth is None:
            raise RuntimeError("Codex auth missing. Run Codex login first, or use provider=openai with an API key.")

        payload = self._build_compact_payload(messages, tools)
        response = self._post_compact_payload(payload)
        if response.status_code in {401, 403} and self.auth.refresh_token:
            response.close()
            try:
                self.auth = refresh_codex_auth(auth=self.auth)
            except PermanentCodexAuthError:
                raise
            except CodexAuthError as exc:
                raise RuntimeError(f"Codex auth refresh failed after HTTP {response.status_code}: {exc}") from exc
            response = self._post_compact_payload(payload)
        try:
            if response.status_code >= 400:
                raise RuntimeError(f"Codex compact request failed: HTTP {response.status_code}: {response.text[:1000]}")
            try:
                data = response.json()
            except json.JSONDecodeError as exc:
                raise RuntimeError(f"Codex compact response was not JSON: {response.text[:1000]}") from exc
        finally:
            response.close()

        output = data.get("output") if isinstance(data, dict) else data
        if not isinstance(output, list):
            raise RuntimeError(f"Codex compact response did not include output list: {data}")
        return ProviderCompactionResult(messages=_response_items_to_messages(output))

    def start_turn(
        self,
        messages: List[Dict[str, Any]],
        tools: List[Dict[str, Any]],
    ) -> Iterable[ModelEvent]:
        if self.auth is None:
            raise RuntimeError("Codex auth missing. Run Codex login first, or use provider=openai with an API key.")

        payload = self._build_payload(messages, tools)
        response = self._post_payload(payload)
        if response.status_code in {401, 403} and self.auth.refresh_token:
            response.close()
            try:
                self.auth = refresh_codex_auth(auth=self.auth)
            except PermanentCodexAuthError:
                raise
            except CodexAuthError as exc:
                raise RuntimeError(f"Codex auth refresh failed after HTTP {response.status_code}: {exc}") from exc
            response = self._post_payload(payload)
        if response.status_code >= 400:
            raise RuntimeError(f"Codex Responses request failed: HTTP {response.status_code}: {response.text[:1000]}")

        seen_tool_calls: Set[str] = set()
        try:
            for event in _iter_sse_json(response):
                event_type = event.get("type")
                if event_type == "response.created":
                    continue
                elif event_type == "response.output_text.delta":
                    delta = event.get("delta")
                    if delta:
                        yield ModelEvent.text(str(delta))
                elif event_type == "response.output_item.done":
                    item = event.get("item") or {}
                    if isinstance(item, dict) and item.get("type") == "function_call":
                        call_id = str(item.get("call_id") or item.get("id"))
                        if call_id not in seen_tool_calls:
                            seen_tool_calls.add(call_id)
                            yield self._event_from_function_call(item)
                elif event_type in {"response.completed", "response.done", "response.incomplete"}:
                    response_obj = event.get("response") or {}
                    if isinstance(response_obj, dict):
                        for item in response_obj.get("output") or []:
                            if item.get("type") == "message":
                                yield from self._events_from_message(item)
                            elif item.get("type") == "function_call":
                                call_id = str(item.get("call_id") or item.get("id"))
                                if call_id not in seen_tool_calls:
                                    seen_tool_calls.add(call_id)
                                    yield self._event_from_function_call(item)
                        usage = ModelTokenUsage.from_openai_usage(response_obj.get("usage"))
                        if usage is not None:
                            yield ModelEvent.usage(usage, model=self.model, provider="codex")
                    return
                elif event_type == "error":
                    raise RuntimeError(f"Codex stream error: {event}")
        finally:
            response.close()

    def _build_payload(self, messages: List[Dict[str, Any]], tools: List[Dict[str, Any]]) -> Dict[str, Any]:
        input_items = trim_visual_context_images(self._convert_messages(messages))

        payload: Dict[str, Any] = {
            "model": self.model,
            "input": input_items,
            "store": False,
            "stream": True,
            "instructions": self.instructions,
            "text": {"verbosity": "low"},
            "tool_choice": "auto",
            "parallel_tool_calls": True,
        }
        if tools:
            payload["tools"] = tools
        return payload

    def _build_compact_payload(self, messages: List[Dict[str, Any]], tools: List[Dict[str, Any]]) -> Dict[str, Any]:
        input_items = trim_visual_context_images(self._convert_messages(messages))
        payload: Dict[str, Any] = {
            "model": self.model,
            "input": input_items,
            "instructions": self.instructions,
            "text": {"verbosity": "low"},
            "parallel_tool_calls": True,
        }
        if tools:
            payload["tools"] = tools
        return payload

    def _post_payload(self, payload: Dict[str, Any]) -> requests.Response:
        assert self.auth is not None
        last_exc: Optional[BaseException] = None
        transient_statuses = {408, 409, 429, 500, 502, 503, 504}
        for attempt in range(self.max_retries + 1):
            try:
                response = requests.post(
                    _codex_url(self.base_url),
                    headers={
                        "Authorization": f"Bearer {self.auth.access_token}",
                        "chatgpt-account-id": self.auth.account_id,
                        "originator": "llm-browser",
                        "OpenAI-Beta": "responses=experimental",
                        "Content-Type": "application/json",
                        "Accept": "text/event-stream",
                    },
                    json=payload,
                    timeout=self.timeout_s,
                    stream=True,
                )
                if response.status_code not in transient_statuses or attempt >= self.max_retries:
                    return response
                response.close()
            except requests.RequestException as exc:
                last_exc = exc
                if attempt >= self.max_retries:
                    raise
            time.sleep(min(2.0, 0.25 * (2**attempt)))
        if last_exc is not None:
            raise last_exc
        raise RuntimeError("Codex Responses request failed before receiving a response")

    def _post_compact_payload(self, payload: Dict[str, Any]) -> requests.Response:
        assert self.auth is not None
        return requests.post(
            _codex_compact_url(self.base_url),
            headers={
                "Authorization": f"Bearer {self.auth.access_token}",
                "chatgpt-account-id": self.auth.account_id,
                "originator": "llm-browser",
                "OpenAI-Beta": "responses=experimental",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
            json=payload,
            timeout=self.timeout_s,
        )

    def _convert_messages(self, messages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        input_items: List[Dict[str, Any]] = []
        known_tool_call_ids: Set[str] = set()
        for message in messages:
            role = message.get("role")
            if role == "user":
                input_items.append(
                    {
                        "role": "user",
                        "content": [{"type": "input_text", "text": str(message.get("content", ""))}],
                    }
                )
            elif role == "assistant":
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
                if isinstance(item, dict):
                    input_items.append(deepcopy(item))
        return input_items

    def _events_from_message(self, item: Dict[str, Any]) -> Iterable[ModelEvent]:
        for content in item.get("content") or []:
            if content.get("type") in {"output_text", "text"} and content.get("text"):
                yield ModelEvent.text(str(content["text"]))

    def _event_from_function_call(self, item: Dict[str, Any]) -> ModelEvent:
        try:
            arguments = json.loads(item.get("arguments") or "{}")
        except json.JSONDecodeError:
            arguments = {"_raw": str(item.get("arguments") or "")}
        call_id = item.get("call_id") or item.get("id")
        if not call_id:
            raise RuntimeError(f"function_call item is missing call_id: {item}")
        return ModelEvent.call(ToolCall(id=str(call_id), name=str(item["name"]), arguments=arguments))


def _codex_url(base_url: str) -> str:
    normalized = base_url.rstrip("/")
    if normalized.endswith("/codex/responses"):
        return normalized
    if normalized.endswith("/codex"):
        return f"{normalized}/responses"
    return f"{normalized}/codex/responses"


def _codex_compact_url(base_url: str) -> str:
    responses_url = _codex_url(base_url)
    return f"{responses_url.rstrip('/')}/compact"


def _response_items_to_messages(items: List[Any]) -> List[Dict[str, Any]]:
    messages: List[Dict[str, Any]] = []
    for item in items:
        if not isinstance(item, dict):
            continue
        item_type = item.get("type")
        if item_type == "message":
            role = str(item.get("role") or "")
            text = _response_message_text(item)
            if role in {"user", "assistant"} and text:
                messages.append({"role": role, "content": text})
            continue
        if item_type in {"compaction", "context_compaction"}:
            messages.append({"role": "provider_item", "provider": "responses", "item": deepcopy(item)})
    return messages


def _response_message_text(item: Dict[str, Any]) -> str:
    pieces: List[str] = []
    for content in item.get("content") or []:
        if not isinstance(content, dict):
            continue
        text = content.get("text")
        if text:
            pieces.append(str(text))
    return "\n".join(pieces)


def _iter_sse_json(response: requests.Response):
    data_lines: List[str] = []
    for raw_line in response.iter_lines(decode_unicode=True):
        if isinstance(raw_line, bytes):
            line = raw_line.decode("utf-8", errors="replace")
        else:
            line = raw_line or ""
        if line == "":
            if data_lines:
                data = "\n".join(data_lines).strip()
                data_lines = []
                if data and data != "[DONE]":
                    yield json.loads(data)
            continue
        if line.startswith("data:"):
            data_lines.append(line[5:].strip())
    if data_lines:
        data = "\n".join(data_lines).strip()
        if data and data != "[DONE]":
            yield json.loads(data)
