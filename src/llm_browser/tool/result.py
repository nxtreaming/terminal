from __future__ import annotations

import base64
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Union


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

    def to_provider_content(self) -> Union[str, List[Dict[str, Any]]]:
        if self.images:
            content: List[Dict[str, Any]] = []
            text = self._text_summary()
            if text:
                content.append({"type": "input_text", "text": text})
            for image in self.images:
                try:
                    data = base64.b64encode(Path(image.path).read_bytes()).decode("ascii")
                    content.append(
                        {
                            "type": "input_image",
                            "detail": image.detail,
                            "image_url": f"data:{image.mime_type};base64,{data}",
                        }
                    )
                except OSError:
                    content.append({"type": "input_text", "text": f"[missing image artifact: {image.path}]"})
            return content
        return self._text_summary()

    def _text_summary(self) -> str:
        parts: List[str] = []
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
