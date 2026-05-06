from __future__ import annotations

import json
import mimetypes
from io import BytesIO
from pathlib import Path
from typing import Any, Dict, Optional

from llm_browser.events.event import now_ms
from llm_browser.harness.api import HelperAPI, safe_artifact_name
from llm_browser.tool.result import ToolImage


SKILL = {
    "name": "artifacts",
    "description": "Local artifact, download, PDF, and image attachment helpers.",
    "exports": [
        "save_artifact",
        "download_file",
        "read_pdf_text",
        "attach_image",
    ],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    def save_artifact(name: str, content: Any = None, mode: str = "text") -> str:
        source = Path(name).expanduser()
        resolved_source = source if source.is_absolute() else api.cwd / source
        if content is None and resolved_source.exists():
            return str(api.copy_to_artifact(resolved_source, resolved_source.name))
        safe_name = safe_artifact_name(name)
        path = api.artifact_dir / "python-artifacts" / safe_name
        path.parent.mkdir(parents=True, exist_ok=True)
        if mode == "bytes" or isinstance(content, (bytes, bytearray, memoryview)):
            path.write_bytes(bytes(content))
        else:
            path.write_text(str(content), encoding="utf-8")
        return str(path)

    def download_file(url: str, path: Optional[str] = None, timeout: float = 30.0, headers: Optional[Dict[str, str]] = None) -> str:
        try:
            import requests
        except Exception as exc:
            raise RuntimeError("requests is not installed") from exc

        target = Path(path or Path(url.split("?", 1)[0]).name or "download.bin").expanduser()
        if not target.is_absolute():
            target = api.cwd / target
        target.parent.mkdir(parents=True, exist_ok=True)
        request_headers = {"User-Agent": "Mozilla/5.0"}
        if headers:
            request_headers.update(headers)
        api.check_cancel()
        response = requests.get(url, headers=request_headers, timeout=timeout)
        api.check_cancel()
        response.raise_for_status()
        target.write_bytes(response.content)
        return str(target)

    def read_pdf_text(source: str, max_pages: Optional[int] = None) -> str:
        try:
            from pypdf import PdfReader
        except Exception as exc:
            raise RuntimeError("pypdf is not installed") from exc

        source_path = Path(source).expanduser()
        stream: Any
        close_stream = False
        if source.startswith(("http://", "https://")):
            try:
                import requests
            except Exception as exc:
                raise RuntimeError("requests is not installed") from exc
            api.check_cancel()
            response = requests.get(source, headers={"User-Agent": "Mozilla/5.0"}, timeout=30)
            api.check_cancel()
            response.raise_for_status()
            stream = BytesIO(response.content)
            close_stream = True
        else:
            stream = source_path if source_path.is_absolute() else api.cwd / source_path
        try:
            reader = PdfReader(stream)
            pages = reader.pages[:max_pages] if max_pages is not None else reader.pages
            return "\n".join(page.extract_text() or "" for page in pages)
        finally:
            if close_stream:
                stream.close()

    def attach_image(path: str, label: Optional[str] = None, detail: str = "auto") -> ToolImage:
        image_path = api.resolve_path(path).resolve()
        if not image_path.exists():
            raise FileNotFoundError(str(image_path))
        sidecar = image_path.with_suffix(".json")
        metadata: Dict[str, Any] = {}
        if sidecar.exists():
            try:
                metadata = json.loads(sidecar.read_text(encoding="utf-8"))
            except Exception:
                metadata = {}
        mime = mimetypes.guess_type(str(image_path))[0] or str(metadata.get("mime_type") or "image/png")
        image = ToolImage(
            label=label or str(metadata.get("label") or image_path.stem),
            path=str(image_path),
            mime_type=mime,
            detail=detail or str(metadata.get("detail") or "auto"),
            order=len(api.images) + 1,
            ts_ms=now_ms(),
            url=str(metadata.get("url") or ""),
            title=str(metadata.get("title") or ""),
            viewport=dict(metadata.get("viewport") or {}),
        )
        return api.emit_image(image)

    return {
        "save_artifact": save_artifact,
        "download_file": download_file,
        "read_pdf_text": read_pdf_text,
        "attach_image": attach_image,
    }
