from __future__ import annotations

import uuid
from typing import Any, Dict, Iterable, List

from llm_browser.provider.types import ModelEvent, ToolCall


class FakeProvider:
    """Deterministic provider used for local spine tests."""

    def start_turn(
        self,
        messages: List[Dict[str, Any]],
        tools: List[Dict[str, Any]],
    ) -> Iterable[ModelEvent]:
        tool_outputs = [message for message in messages if message.get("role") == "tool"]
        if not tool_outputs:
            task = str(messages[-1].get("content", ""))
            yield ModelEvent.text("I will echo the task through the tool loop.\n")
            yield ModelEvent.call(
                ToolCall(
                    id=f"call_{uuid.uuid4().hex[:10]}",
                    name="echo",
                    arguments={"text": task},
                )
            )
            return

        last_output = str(tool_outputs[-1].get("content", ""))
        yield ModelEvent.text(f"Tool output received: {last_output}\n")
        yield ModelEvent.call(
            ToolCall(
                id=f"call_{uuid.uuid4().hex[:10]}",
                name="done",
                arguments={"result": last_output},
            )
        )
