from __future__ import annotations

from dataclasses import dataclass

from llm_browser.session.metadata import SessionMetadata
from llm_browser.session.store import SessionStore
from llm_browser.tool.result import ToolImage


@dataclass(frozen=True)
class ToolContext:
    session: SessionMetadata
    store: SessionStore
    tool_call_id: str
    tool_name: str

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
