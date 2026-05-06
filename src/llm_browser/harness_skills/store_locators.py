from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Dict, Optional

from llm_browser.harness.api import HelperAPI
from llm_browser.tool.web_fetch import _extract_store_locator_locations


SKILL = {
    "name": "store_locators",
    "description": "Store-locator extraction recipes, including Bullseye location APIs.",
    "exports": ["extract_store_locator_locations", "store_locator_locations"],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    def extract_store_locator_locations(
        target: str,
        provider: str = "auto",
        country_ids: Optional[Any] = None,
        max_locations: int = 10000,
        timeout: float = 30.0,
        save_to: Optional[str] = None,
        include_locations: bool = True,
    ) -> Dict[str, Any]:
        result = _extract_store_locator_locations(
            target,
            provider=provider,
            country_ids=country_ids,
            max_locations=max_locations,
            timeout=timeout,
        )
        locations = result.get("locations")
        if save_to and isinstance(locations, list):
            target_path = Path(save_to).expanduser()
            if not target_path.is_absolute():
                target_path = api.cwd / target_path
            target_path.parent.mkdir(parents=True, exist_ok=True)
            target_path.write_text(json.dumps(locations, ensure_ascii=False, indent=2), encoding="utf-8")
            result["path"] = str(target_path)
        if not include_locations:
            result.pop("locations", None)
        return result

    return {
        "extract_store_locator_locations": extract_store_locator_locations,
        "store_locator_locations": extract_store_locator_locations,
    }
