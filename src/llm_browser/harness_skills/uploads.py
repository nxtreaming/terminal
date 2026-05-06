from __future__ import annotations

from pathlib import Path
from typing import Any, Dict

from llm_browser.harness.api import HelperAPI


SKILL = {
    "name": "uploads",
    "description": "File input helpers for browser upload workflows using raw CDP DOM.setFileInputFiles.",
    "exports": ["upload_file"],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    return {"upload_file": make_upload_file(api)}


def make_upload_file(api: HelperAPI):
    def upload_file(selector: str, path: Any) -> Dict[str, Any]:
        cdp = api.namespace["cdp"]
        files = path if isinstance(path, (list, tuple)) else [path]
        normalized_files = []
        for item in files:
            file_path = Path(str(item)).expanduser()
            if not file_path.is_absolute():
                file_path = api.cwd / file_path
            normalized_files.append(str(file_path.resolve()))
        document = cdp("DOM.getDocument", depth=-1)
        root = document.get("root") if isinstance(document.get("root"), dict) else {}
        node_id = cdp("DOM.querySelector", nodeId=root.get("nodeId"), selector=selector).get("nodeId")
        if not node_id:
            raise RuntimeError(f"no element for {selector}")
        return cdp("DOM.setFileInputFiles", files=normalized_files, nodeId=node_id)

    return upload_file
