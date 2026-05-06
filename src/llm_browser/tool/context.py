from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Dict, List, Optional

from llm_browser.session.cancel import SessionCancelled
from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tool.result import ToolImage


@dataclass(frozen=True)
class ToolContext:
    session: SessionMetadata
    store: SessionStore
    tool_call_id: str
    tool_name: str
    conversation_messages: Optional[List[Dict[str, Any]]] = None

    def is_cancel_requested(self) -> bool:
        return self.store.is_cancel_requested(self.session.id)

    def check_cancel(self) -> None:
        request = self.store.cancel_request(self.session.id)
        if request is not None:
            raise SessionCancelled(self.session.id, request["reason"])

    def emit_image(self, image: ToolImage) -> None:
        self.store.emit(
            self.session.id,
            "tool.image",
            {
                "tool_call_id": self.tool_call_id,
                "name": self.tool_name,
                "image": image.to_dict(),
            },
        )

    def emit_output(self, text: str, stream: str = "stdout") -> None:
        self.store.emit(
            self.session.id,
            "tool.output",
            {
                "tool_call_id": self.tool_call_id,
                "name": self.tool_name,
                "stream": stream,
                "text": text,
            },
        )
