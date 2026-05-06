from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Dict, Optional


def browser_use_api_key() -> Optional[str]:
    return os.environ.get("BROWSER_USE_API_KEY") or os.environ.get("BU_API_KEY")


def browser_use_api_base() -> str:
    return (
        os.environ.get("BROWSER_USE_API_BASE_URL")
        or os.environ.get("BROWSER_USE_API_BASE")
        or "https://api.browser-use.com/api/v3"
    ).rstrip("/")


def upload_to_browser_use_cloud(path: Path, filename: str, content_type: str, api_key: str) -> Dict[str, Any]:
    try:
        import requests
    except Exception as exc:
        raise RuntimeError("requests is not installed") from exc

    base_url = browser_use_api_base()
    headers = {"X-Browser-Use-API-Key": api_key}
    session_response = requests.post(f"{base_url}/sessions", headers=headers, json={"keep_alive": True}, timeout=30)
    session_response.raise_for_status()
    session_data = session_response.json()
    session_id = str(session_data.get("id") or session_data.get("sessionId") or session_data.get("session_id") or "")
    if not session_id:
        raise RuntimeError(f"Browser Use session response did not include an id: {session_data}")

    upload_payload = {"files": [{"name": filename, "contentType": content_type}]}
    upload_response = requests.post(
        f"{base_url}/sessions/{session_id}/files/upload",
        headers=headers,
        json=upload_payload,
        timeout=30,
    )
    if upload_response.status_code == 422:
        upload_payload = {"files": [{"name": filename, "content_type": content_type}]}
        upload_response = requests.post(
            f"{base_url}/sessions/{session_id}/files/upload",
            headers=headers,
            json=upload_payload,
            timeout=30,
        )
    upload_response.raise_for_status()
    upload_data = upload_response.json()
    files = upload_data.get("files") or []
    if not files:
        raise RuntimeError(f"Browser Use upload response did not include files: {upload_data}")
    uploaded = files[0]
    upload_url = uploaded.get("uploadUrl") or uploaded.get("upload_url")
    remote_path = uploaded.get("path") or uploaded.get("filePath") or uploaded.get("file_path") or filename
    if not upload_url:
        raise RuntimeError(f"Browser Use upload response did not include uploadUrl: {upload_data}")

    put_response = requests.put(upload_url, data=path.read_bytes(), headers={"Content-Type": content_type}, timeout=60)
    put_response.raise_for_status()

    list_response = requests.get(
        f"{base_url}/sessions/{session_id}/files",
        headers=headers,
        params={"includeUrls": "true", "prefix": remote_path, "limit": 10},
        timeout=30,
    )
    list_response.raise_for_status()
    list_data = list_response.json()
    for item in list_data.get("files", []):
        item_path = str(item.get("path") or "")
        if item_path == remote_path or item_path.endswith(f"/{filename}") or item_path.endswith(filename):
            download_url = item.get("url") or item.get("downloadUrl") or item.get("download_url")
            if download_url:
                return {
                    "browserUseSessionId": session_id,
                    "remotePath": item_path or remote_path,
                    "downloadUrl": download_url,
                }
    raise RuntimeError(f"Browser Use file list did not include a download URL for {remote_path}: {list_data}")
