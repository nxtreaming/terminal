from __future__ import annotations

import mimetypes
from pathlib import Path
from typing import Any, Dict, Optional

from llm_browser.harness.api import HelperAPI, safe_artifact_name
from llm_browser.harness_skills.artifacts import install as install_artifacts
from llm_browser.tool.browser_artifacts import browser_use_api_key, upload_to_browser_use_cloud


SKILL = {
    "name": "cloud_artifacts",
    "description": "Browser Use cloud artifact upload and shareable download URL helpers.",
    "exports": ["upload_artifact", "create_download_url", "artifact_download_url"],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    save_artifact = api.namespace.get("save_artifact")
    if not callable(save_artifact):
        save_artifact = install_artifacts(api)["save_artifact"]

    def upload_artifact(path: str, filename: Optional[str] = None, content_type: Optional[str] = None) -> Dict[str, Any]:
        source = api.resolve_path(path).resolve()
        if not source.exists():
            raise FileNotFoundError(str(source))
        artifact_path = Path(save_artifact(str(source)))
        upload_name = safe_artifact_name(filename or artifact_path.name)
        mime = content_type or mimetypes.guess_type(upload_name)[0] or "application/octet-stream"
        local_url = artifact_path.as_uri()
        api_key = browser_use_api_key()
        if not api_key:
            return {
                "filename": upload_name,
                "path": str(artifact_path),
                "downloadUrl": local_url,
                "cloud": False,
                "note": "BROWSER_USE_API_KEY is not set; returning local file URL.",
            }
        try:
            cloud = upload_to_browser_use_cloud(artifact_path, filename=upload_name, content_type=mime, api_key=api_key)
        except Exception as exc:
            return {
                "filename": upload_name,
                "path": str(artifact_path),
                "downloadUrl": local_url,
                "cloud": False,
                "error": str(exc),
                "note": "Browser Use upload failed; returning local file URL.",
            }
        return {"filename": upload_name, "path": str(artifact_path), "downloadUrl": cloud["downloadUrl"], "cloud": True, **cloud}

    def create_download_url(path: str, filename: Optional[str] = None, content_type: Optional[str] = None) -> str:
        return str(upload_artifact(path, filename=filename, content_type=content_type)["downloadUrl"])

    return {
        "upload_artifact": upload_artifact,
        "create_download_url": create_download_url,
        "artifact_download_url": create_download_url,
    }
