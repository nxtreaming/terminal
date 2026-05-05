from __future__ import annotations

from typing import Any, Dict, Iterable, List, Protocol

from llm_browser.provider.types import ModelEvent


class Provider(Protocol):
    def start_turn(
        self,
        messages: List[Dict[str, Any]],
        tools: List[Dict[str, Any]],
    ) -> Iterable[ModelEvent]:
        ...
