from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Dict, Optional


@dataclass(frozen=True)
class ToolCall:
    id: str
    name: str
    arguments: Dict[str, Any]


@dataclass(frozen=True)
class ModelEvent:
    type: str
    text: str = ""
    tool_call: Optional[ToolCall] = None

    @classmethod
    def text(cls, text: str) -> "ModelEvent":
        return cls(type="text_delta", text=text)

    @classmethod
    def call(cls, tool_call: ToolCall) -> "ModelEvent":
        return cls(type="tool_call", tool_call=tool_call)

    @classmethod
    def done(cls) -> "ModelEvent":
        return cls(type="done")
