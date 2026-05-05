from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List


@dataclass(frozen=True)
class ToolImage:
    label: str
    path: str
    mime_type: str = "image/png"
    detail: str = "auto"
    order: int = 0
    ts_ms: int = 0
    url: str = ""
    title: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "label": self.label,
            "path": self.path,
            "mime_type": self.mime_type,
            "detail": self.detail,
            "order": self.order,
            "ts_ms": self.ts_ms,
            "url": self.url,
            "title": self.title,
        }


@dataclass(frozen=True)
class ToolResult:
    text: str = ""
    images: List[ToolImage] = field(default_factory=list)
    data: Dict[str, Any] = field(default_factory=dict)

    def to_provider_content(self) -> str:
        # Image transport is provider-specific. For the fake provider and event log,
        # keep a compact text representation until the OpenAI provider is wired.
        parts = []
        if self.text:
            parts.append(self.text)
        if self.data:
            parts.append(f"data={self.data}")
        if self.images:
            labels = ", ".join(image.label for image in self.images)
            parts.append(f"images=[{labels}]")
        return "\n".join(parts)

    def to_event_payload(self) -> Dict[str, Any]:
        return {
            "text": self.text,
            "data": self.data,
            "images": [image.to_dict() for image in self.images],
        }
