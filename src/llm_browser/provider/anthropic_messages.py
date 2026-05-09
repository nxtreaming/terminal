from __future__ import annotations

import json
import time
from typing import Any, Dict, Iterable, List, Optional

import requests

from llm_browser.auth.anthropic import is_anthropic_oauth_token
from llm_browser.browser.instructions import BROWSER_AGENT_INSTRUCTIONS
from llm_browser.provider.types import ModelEvent, ToolCall
from llm_browser.session.usage import ModelTokenUsage


ANTHROPIC_BASE_URL = "https://api.anthropic.com"
CLAUDE_CODE_VERSION = "2.1.75"
CLAUDE_CODE_TOOLS = {
    "read": "Read",
    "write": "Write",
    "edit": "Edit",
    "shell": "Bash",
    "bash": "Bash",
    "grep": "Grep",
    "glob": "Glob",
    "todo_write": "TodoWrite",
    "todowrite": "TodoWrite",
    "web_fetch": "WebFetch",
    "webfetch": "WebFetch",
    "web_search": "WebSearch",
    "websearch": "WebSearch",
}


class AnthropicMessagesProvider:
    """Anthropic Messages provider with API-key and Claude Code OAuth support."""

    def __init__(
        self,
        *,
        api_key: Optional[str],
        model: str,
        credential_type: str = "api_key",
        base_url: str = ANTHROPIC_BASE_URL,
        timeout_s: float = 180.0,
        max_retries: int = 2,
        max_tokens: int = 16000,
        instructions: Optional[str] = None,
        thinking_mode: str = "adaptive",
        effort: str = "low",
    ) -> None:
        self.api_key = api_key
        self.model = model
        self.credential_type = credential_type
        self.base_url = base_url.rstrip("/")
        self.timeout_s = timeout_s
        self.max_retries = max(0, int(max_retries))
        self.max_tokens = max_tokens
        self.provider_label = "anthropic"
        self.instructions = instructions or BROWSER_AGENT_INSTRUCTIONS
        self.thinking_mode = thinking_mode
        self.effort = effort

    def set_instructions(self, instructions: str) -> None:
        self.instructions = instructions

    def reset_session(self) -> None:
        return None

    def supports_remote_compaction(self) -> bool:
        return False

    @property
    def is_oauth(self) -> bool:
        return self.credential_type == "oauth" or bool(self.api_key and is_anthropic_oauth_token(self.api_key))

    def start_turn(
        self,
        messages: List[Dict[str, Any]],
        tools: List[Dict[str, Any]],
    ) -> Iterable[ModelEvent]:
        if not self.api_key:
            raise RuntimeError("Anthropic auth missing. Run `but auth anthropic login` or set ANTHROPIC_API_KEY.")

        payload = self._build_payload(messages, tools)
        response = self._post_payload(payload)
        if response.status_code >= 400:
            raise RuntimeError(f"Anthropic request failed: HTTP {response.status_code}: {response.text[:1000]}")
        data = response.json()
        for block in data.get("content") or []:
            block_type = block.get("type")
            if block_type == "text" and block.get("text"):
                yield ModelEvent.text(str(block["text"]))
            elif block_type == "tool_use":
                yield self._event_from_tool_use(block, tools)
        usage = _anthropic_usage(data.get("usage"))
        if usage is not None:
            yield ModelEvent.usage(usage, model=self.model, provider="anthropic")

    def _post_payload(self, payload: Dict[str, Any]) -> requests.Response:
        last_exc: Optional[BaseException] = None
        transient_statuses = {408, 409, 429, 500, 502, 503, 504}
        for attempt in range(self.max_retries + 1):
            try:
                response = requests.post(
                    f"{self.base_url}/v1/messages",
                    headers=self._headers(),
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
        raise RuntimeError("Anthropic request failed before receiving a response")

    def _headers(self) -> Dict[str, str]:
        beta_features = ["fine-grained-tool-streaming-2025-05-14", "interleaved-thinking-2025-05-14"]
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json",
            "anthropic-version": "2023-06-01",
            "anthropic-dangerous-direct-browser-access": "true",
        }
        if self.is_oauth:
            headers.update(
                {
                    "Authorization": f"Bearer {self.api_key}",
                    "anthropic-beta": ",".join(["claude-code-20250219", "oauth-2025-04-20", *beta_features]),
                    "user-agent": f"claude-cli/{CLAUDE_CODE_VERSION}",
                    "x-app": "cli",
                }
            )
        else:
            headers.update({"x-api-key": str(self.api_key), "anthropic-beta": ",".join(beta_features)})
        return headers

    def _build_payload(self, messages: List[Dict[str, Any]], tools: List[Dict[str, Any]]) -> Dict[str, Any]:
        payload: Dict[str, Any] = {
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": self._system_blocks(),
            "messages": self._convert_messages(messages),
        }
        if tools:
            payload["tools"] = [self._convert_tool(tool) for tool in tools]
            payload["tool_choice"] = {"type": "auto"}
        if self.thinking_mode == "adaptive" and self._supports_adaptive_thinking():
            payload["thinking"] = {"type": "adaptive"}
            payload["output_config"] = {"effort": self.effort}
        elif self.thinking_mode == "enabled":
            payload["thinking"] = {"type": "enabled", "budget_tokens": 4096}
        return payload

    def _system_blocks(self) -> List[Dict[str, Any]]:
        blocks: List[Dict[str, Any]] = []
        if self.is_oauth:
            blocks.append(
                {
                    "type": "text",
                    "text": "You are Claude Code, Anthropic's official CLI for Claude.",
                    "cache_control": {"type": "ephemeral"},
                }
            )
        blocks.append({"type": "text", "text": self.instructions, "cache_control": {"type": "ephemeral"}})
        return blocks

    def _convert_messages(self, messages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        converted: List[Dict[str, Any]] = []
        i = 0
        while i < len(messages):
            message = messages[i]
            role = message.get("role")
            if role == "user":
                converted.append({"role": "user", "content": _anthropic_content_blocks(message.get("content", ""))})
            elif role == "assistant":
                blocks: List[Dict[str, Any]] = []
                content = message.get("content")
                if content:
                    blocks.append({"type": "text", "text": str(content)})
                for call in message.get("tool_calls") or []:
                    blocks.append(
                        {
                            "type": "tool_use",
                            "id": str(call["id"]),
                            "name": self._request_tool_name(str(call["name"])),
                            "input": call.get("arguments") or {},
                        }
                    )
                if blocks:
                    converted.append({"role": "assistant", "content": blocks})
            elif role == "tool":
                tool_results: List[Dict[str, Any]] = []
                while i < len(messages) and messages[i].get("role") == "tool":
                    tool_message = messages[i]
                    tool_results.append(
                        {
                            "type": "tool_result",
                            "tool_use_id": str(tool_message["tool_call_id"]),
                            "content": _anthropic_content_blocks(tool_message.get("content", "")),
                            "is_error": False,
                        }
                    )
                    i += 1
                converted.append({"role": "user", "content": tool_results})
                continue
            elif role == "provider_item":
                converted.append(
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "text",
                                "text": "[provider-specific compacted context item retained from prior history]",
                            }
                        ],
                    }
                )
            i += 1
        _cache_last_user_block(converted)
        return converted

    def _convert_tool(self, tool: Dict[str, Any]) -> Dict[str, Any]:
        schema = dict(tool.get("parameters") or {"type": "object", "properties": {}})
        schema.pop("title", None)
        return {
            "name": self._request_tool_name(str(tool["name"])),
            "description": str(tool.get("description") or ""),
            "input_schema": schema,
        }

    def _request_tool_name(self, name: str) -> str:
        if not self.is_oauth:
            return name
        return CLAUDE_CODE_TOOLS.get(name.lower(), name)

    def _response_tool_name(self, name: str, tools: List[Dict[str, Any]]) -> str:
        lower = name.lower()
        for tool in tools:
            original = str(tool.get("name") or "")
            if lower == original.lower() or lower == self._request_tool_name(original).lower():
                return original
        if lower == "bash":
            return "shell"
        return name

    def _event_from_tool_use(self, block: Dict[str, Any], tools: List[Dict[str, Any]]) -> ModelEvent:
        raw_input = block.get("input") or {}
        arguments = raw_input if isinstance(raw_input, dict) else {"_raw": raw_input}
        return ModelEvent.call(
            ToolCall(
                id=str(block.get("id") or ""),
                name=self._response_tool_name(str(block.get("name") or ""), tools),
                arguments=arguments,
            )
        )

    def _supports_adaptive_thinking(self) -> bool:
        lowered = self.model.lower()
        return lowered.startswith("claude-opus-4") or lowered.startswith("claude-sonnet-4")


def _anthropic_content_blocks(content: Any) -> List[Dict[str, Any]]:
    if isinstance(content, list):
        blocks: List[Dict[str, Any]] = []
        for item in content:
            if not isinstance(item, dict):
                blocks.append({"type": "text", "text": str(item)})
            elif item.get("type") == "input_text":
                text = str(item.get("text") or "")
                if text:
                    blocks.append({"type": "text", "text": text})
            elif item.get("type") == "input_image":
                image_url = str(item.get("image_url") or "")
                image_block = _anthropic_image_block(image_url)
                if image_block is not None:
                    blocks.append(image_block)
        return blocks or [{"type": "text", "text": ""}]
    return [{"type": "text", "text": str(content)}]


def _anthropic_image_block(image_url: str) -> Optional[Dict[str, Any]]:
    if not image_url:
        return None
    if image_url.startswith("data:image/") and ";base64," in image_url:
        header, data = image_url.split(";base64,", 1)
        media_type = header.replace("data:", "") or "image/png"
        return {"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}
    return {"type": "image", "source": {"type": "url", "url": image_url}}


def _cache_last_user_block(messages: List[Dict[str, Any]]) -> None:
    for message in reversed(messages):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, list) and content:
            block = content[-1]
            if isinstance(block, dict) and block.get("type") in {"text", "image", "tool_result"}:
                block["cache_control"] = {"type": "ephemeral"}
        return


def _anthropic_usage(usage: Any) -> Optional[ModelTokenUsage]:
    if not isinstance(usage, dict):
        return None
    payload = {
        "input_tokens": usage.get("input_tokens") or 0,
        "output_tokens": usage.get("output_tokens") or 0,
        "cached_input_tokens": usage.get("cache_read_input_tokens") or 0,
        "cache_creation_input_tokens": usage.get("cache_creation_input_tokens") or 0,
        "total_tokens": (usage.get("input_tokens") or 0) + (usage.get("output_tokens") or 0),
    }
    return ModelTokenUsage.from_openai_usage(payload)
