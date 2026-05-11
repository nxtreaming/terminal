from __future__ import annotations

import contextlib
import atexit
import base64
import importlib
import io
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import traceback
import mimetypes
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any, Dict


_namespaces: Dict[str, Dict[str, Any]] = {}
_managed_chrome: subprocess.Popen[Any] | None = None
_managed_chrome_profile: Path | None = None


class _ToolTimeoutError(TimeoutError):
    pass


def _raise_tool_timeout(signum: int, frame: Any) -> None:
    raise _ToolTimeoutError("python tool timed out")


def _jsonable(value: Any) -> Any:
    try:
        json.dumps(value)
        return value
    except TypeError:
        return repr(value)


def _browser_mode() -> str:
    return os.environ.get("LLM_BROWSER_BROWSER_MODE", "").lower().replace("_", "-").replace(" ", "-")


def _pick_chromium_path() -> str:
    if path := os.environ.get("CHROME_PATH"):
        return path

    # Do not attach automated tests to the user's personal Chrome profile.
    # Recent Chrome builds show a blocking "Allow remote debugging?" prompt
    # for CDP. Homebrew/system Chromium builds can also be quarantined or
    # otherwise blocked by macOS Gatekeeper, so managed browser mode defaults
    # to Playwright's bundled testing browser and requires explicit opt-in for
    # system browsers.
    allow_system_chromium = os.environ.get("LLM_BROWSER_ALLOW_SYSTEM_CHROMIUM") == "1"
    allow_google_chrome = os.environ.get("LLM_BROWSER_ALLOW_GOOGLE_CHROME") == "1"
    candidates = _playwright_chromium_candidates()
    if allow_system_chromium:
        candidates.extend(
            [
                "/opt/homebrew/Caskroom/chromium/latest/chrome-mac/Chromium.app/Contents/MacOS/Chromium",
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
                "/opt/homebrew/bin/chromium",
                "/usr/local/bin/chromium",
                "chromium",
                "chromium-browser",
            ]
        )
    if allow_google_chrome:
        candidates.extend(
            [
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "google-chrome",
                "google-chrome-stable",
            ]
        )
    for candidate in candidates:
        path = Path(candidate)
        if path.is_absolute():
            resolved = path
        else:
            found = shutil.which(candidate)
            if not found:
                continue
            resolved = Path(found)
        if not resolved.exists():
            continue
        if not allow_google_chrome and _looks_like_google_chrome_wrapper(resolved):
            continue
        return str(resolved)
    raise RuntimeError(
        "Playwright Chromium not found; run `python -m playwright install chromium`, "
        "set CHROME_PATH explicitly, use Browser Use cloud, or opt into system Chromium "
        "with LLM_BROWSER_ALLOW_SYSTEM_CHROMIUM=1"
    )


def _playwright_chromium_candidates() -> list[str]:
    roots = [
        Path.home() / "Library/Caches/ms-playwright",
        Path.home() / ".cache/ms-playwright",
    ]
    matches: list[Path] = []
    for root in roots:
        if not root.exists():
            continue
        matches.extend(
            root.glob("chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing")
        )
    return [str(path) for path in sorted(matches, reverse=True)]


def _looks_like_google_chrome_wrapper(path: Path) -> bool:
    try:
        if not path.is_file() or path.stat().st_size > 4096:
            return False
        return "Google Chrome.app" in path.read_text(errors="ignore")
    except OSError:
        return False


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _cleanup_managed_chrome() -> None:
    global _managed_chrome, _managed_chrome_profile
    proc = _managed_chrome
    _managed_chrome = None
    if proc is not None and proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
    if _managed_chrome_profile is not None:
        shutil.rmtree(_managed_chrome_profile, ignore_errors=True)
        _managed_chrome_profile = None


def _ensure_managed_chrome() -> None:
    global _managed_chrome, _managed_chrome_profile
    if os.environ.get("BU_CDP_URL") or os.environ.get("BU_CDP_WS") or os.environ.get("BU_BROWSER_ID"):
        return
    if _browser_mode() not in {"headless", "headless-chromium"} and os.environ.get("LLM_BROWSER_AUTO_CHROME") != "1":
        return
    if _managed_chrome is not None and _managed_chrome.poll() is None:
        return

    port = _free_port()
    profile = Path(tempfile.mkdtemp(prefix="but-managed-chrome."))
    chrome = _pick_chromium_path()
    proc = subprocess.Popen(
        [
            chrome,
            "--headless=new",
            "--remote-debugging-address=127.0.0.1",
            f"--remote-debugging-port={port}",
            f"--user-data-dir={profile}",
            "--no-first-run",
            "--no-default-browser-check",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    deadline = time.time() + 20
    last_error: Exception | None = None
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError("managed Chrome exited before DevTools became available")
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=0.5).read()
            break
        except Exception as exc:  # pragma: no cover - timing/environment dependent
            last_error = exc
            time.sleep(0.25)
    else:
        proc.terminate()
        shutil.rmtree(profile, ignore_errors=True)
        raise RuntimeError(f"managed Chrome DevTools did not become available: {last_error}")

    _managed_chrome = proc
    _managed_chrome_profile = profile
    os.environ["BU_CDP_URL"] = f"http://127.0.0.1:{port}"
    atexit.register(_cleanup_managed_chrome)


def _ensure_cloud_browser(admin: Any) -> None:
    if _browser_mode() != "cloud":
        return
    if os.environ.get("BU_CDP_URL") or os.environ.get("BU_CDP_WS") or os.environ.get("BU_BROWSER_ID"):
        return
    if not os.environ.get("BROWSER_USE_API_KEY"):
        raise RuntimeError("Browser Use cloud selected, but BROWSER_USE_API_KEY is not set")
    name = os.environ.get("BU_NAME", "default")
    if admin.daemon_alive(name):
        return
    browser = admin.start_remote_daemon(name=name)
    live_url = browser.get("liveUrl")
    if live_url:
        os.environ["LLM_BROWSER_LIVE_URL"] = str(live_url)


def _load_browser_harness(ns: Dict[str, Any]) -> None:
    if ns.get("__browser_harness_checked__"):
        return
    ns["__browser_harness_checked__"] = True
    try:
        _ensure_managed_chrome()
        admin = importlib.import_module("browser_harness.admin")
        _ensure_cloud_browser(admin)
        helpers = importlib.import_module("browser_harness.helpers")
        _patch_browser_harness_cdp(helpers, admin)
        names = getattr(helpers, "__all__", None) or [name for name in dir(helpers) if not name.startswith("_")]
        ns.update({name: getattr(helpers, name) for name in names})
        ns["ensure_browser_connection"] = admin.ensure_daemon
        ns["browser_daemon_alive"] = admin.daemon_alive
        ns["__browser_harness_helpers__"] = helpers
        ns["__browser_harness_admin__"] = admin
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None
    except Exception as exc:  # pragma: no cover - environment dependent
        ns["browser_harness_available"] = False
        ns["browser_harness_error"] = str(exc)


def _patch_browser_harness_cdp(helpers: Any, admin: Any) -> None:
    if getattr(helpers, "__llm_browser_cdp_patched__", False):
        return
    original_cdp = helpers.cdp

    def cdp_with_daemon(method: str, session_id: Any = None, **params: Any) -> Any:
        admin.ensure_daemon()
        return original_cdp(method, session_id=session_id, **params)

    helpers.__llm_browser_original_cdp__ = original_cdp
    helpers.__llm_browser_cdp_patched__ = True
    helpers.cdp = cdp_with_daemon


def _emit_protocol_event(request_id: str, event: str, payload: Dict[str, Any]) -> None:
    print(
        json.dumps(
            {
                "id": request_id,
                "event": event,
                "payload": _jsonable(payload),
            },
            ensure_ascii=False,
            separators=(",", ":"),
        ),
        file=sys.__stdout__,
        flush=True,
    )


def _record_browser_event(
    ns: Dict[str, Any],
    request_id: str,
    event_type: str,
    payload: Dict[str, Any],
) -> Dict[str, Any]:
    record = {"type": event_type, "payload": payload}
    ns.setdefault("browser_events", []).append(record)
    _emit_protocol_event(request_id, "browser", record)
    return record


def _namespace(session_id: str, cwd: Path, artifact_dir: Path) -> Dict[str, Any]:
    ns = _namespaces.setdefault(
        session_id,
        {
            "__name__": "__browser_use_worker__",
            "session_id": session_id,
        },
    )
    artifact_dir.mkdir(parents=True, exist_ok=True)
    ns["cwd"] = cwd
    ns["artifact_dir"] = artifact_dir
    ns["result"] = None
    ns["images"] = []
    ns["outputs"] = []
    ns["artifacts"] = []
    ns["browser_events"] = []
    _load_browser_harness(ns)
    return ns


def _safe_name(name: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in name).strip("._") or "artifact"


def _install_host_helpers(ns: Dict[str, Any], request_id: str, cancel_requested: bool) -> None:
    artifact_dir = Path(ns["artifact_dir"])
    if "__browser_harness_capture_screenshot__" not in ns:
        ns["__browser_harness_capture_screenshot__"] = ns.get("capture_screenshot")
    if "__browser_harness_page_info__" not in ns:
        ns["__browser_harness_page_info__"] = ns.get("page_info")

    def emit_output(text: Any) -> None:
        record = {"text": str(text)}
        ns.setdefault("outputs", []).append(record)
        _emit_protocol_event(request_id, "output", record)

    def _copy_artifact(
        path: Any,
        kind: str = "file",
        name: str | None = None,
        mime: str | None = None,
        emit_event: bool = True,
    ) -> Dict[str, Any]:
        source = Path(str(path)).expanduser()
        if not source.is_absolute():
            source = Path.cwd() / source
        if not source.exists() or not source.is_file():
            raise FileNotFoundError(str(source))
        target_dir = artifact_dir / ("images" if kind == "image" else "files")
        target_dir.mkdir(parents=True, exist_ok=True)
        target_name = _safe_name(name or source.name)
        target = target_dir / target_name
        if target.exists():
            stem = target.stem
            suffix = target.suffix
            idx = 2
            while target.exists():
                target = target_dir / f"{stem}-{idx}{suffix}"
                idx += 1
        shutil.copy2(source, target)
        record = {
            "kind": kind,
            "path": str(target),
            "source_path": str(source),
            "mime": mime or mimetypes.guess_type(str(target))[0],
            "bytes": target.stat().st_size,
        }
        ns.setdefault("artifacts", []).append(record)
        if emit_event:
            _emit_protocol_event(request_id, "artifact", record)
        return record

    def copy_artifact(path: Any, kind: str = "file", name: str | None = None, mime: str | None = None) -> Dict[str, Any]:
        return _copy_artifact(path, kind=kind, name=name, mime=mime, emit_event=True)

    def _write_artifact(name: str, content: str, mime: str) -> Dict[str, Any]:
        target_dir = artifact_dir / "files"
        target_dir.mkdir(parents=True, exist_ok=True)
        target_name = _safe_name(name)
        target = target_dir / target_name
        if target.exists():
            stem = target.stem
            suffix = target.suffix
            idx = 2
            while target.exists():
                target = target_dir / f"{stem}-{idx}{suffix}"
                idx += 1
        target.write_text(content, encoding="utf-8")
        record = {
            "kind": "file",
            "path": str(target),
            "source_path": str(target),
            "mime": mime,
            "bytes": target.stat().st_size,
        }
        ns.setdefault("artifacts", []).append(record)
        _emit_protocol_event(request_id, "artifact", record)
        return record

    def emit_image(path: Any, label: str | None = None, detail: str = "auto", mime_type: str | None = None) -> Dict[str, Any]:
        record = _copy_artifact(path, kind="image", mime=mime_type, emit_event=False)
        image = {
            "label": label,
            "path": record["path"],
            "mime_type": record.get("mime") or "image/png",
            "detail": detail,
        }
        ns.setdefault("images", []).append(image)
        _emit_protocol_event(request_id, "image", image)
        return image

    def _screenshot_label(label: str | None = None, path: Any | None = None) -> str:
        if label:
            return str(label)
        if path:
            stem = Path(str(path)).expanduser().stem
            if stem:
                return stem
        return "screenshot"

    def capture_screenshot(
        path: Any | None = None,
        full: bool = False,
        max_dim: int | None = None,
        attach: bool = True,
        label: str | None = None,
        detail: str = "auto",
        mime_type: str | None = None,
        timeout: float | None = None,
        timeout_s: float | None = None,
    ) -> str:
        """Browser-harness capture_screenshot plus direct image attachment.

        Browser-harness itself returns a PNG path. The Rust browser agent keeps
        that contract while defaulting to attach=True so the next model turn
        receives the pixels directly in the tool result.
        """
        _ = timeout, timeout_s  # Accepted for compatibility with the main Python implementation.
        original = ns.get("__browser_harness_capture_screenshot__")
        if not callable(original):
            raise RuntimeError("browser_harness capture_screenshot is not available")
        shot_path = original(path=path, full=full, max_dim=max_dim)
        if attach:
            emit_image(
                shot_path,
                label=_screenshot_label(label=label, path=path),
                detail=detail,
                mime_type=mime_type,
            )
        return str(shot_path)

    def _apply_max_dim(path: Any, max_dim: int | None = None) -> None:
        if not max_dim:
            return
        from PIL import Image

        img = Image.open(path)
        if max(img.size) > max_dim:
            img.thumbnail((max_dim, max_dim))
            img.save(path)

    def screenshot_clip(
        label: str = "screenshot_clip",
        x: float = 0,
        y: float = 0,
        width: float = 1,
        height: float = 1,
        path: Any | None = None,
        scale: float = 1,
        capture_beyond_viewport: bool = False,
        max_dim: int | None = None,
        attach: bool = True,
        detail: str = "auto",
        mime_type: str | None = None,
    ) -> Dict[str, Any] | str:
        """Capture a CSS-pixel viewport/page clip via CDP and optionally attach it.

        This is intentionally CDP-native instead of post-cropping a full PNG.
        Coordinates are CSS pixels, matching `click_at_xy`, not device pixels.
        """
        cdp_fn = ns.get("cdp")
        if not callable(cdp_fn):
            raise RuntimeError("browser_harness cdp is not available")
        if width <= 0 or height <= 0:
            raise ValueError("screenshot_clip width and height must be positive")
        shot_path = Path(str(path)).expanduser() if path is not None else artifact_dir / f"{_safe_name(label)}.png"
        if not shot_path.is_absolute():
            shot_path = Path.cwd() / shot_path
        shot_path.parent.mkdir(parents=True, exist_ok=True)
        response = cdp_fn(
            "Page.captureScreenshot",
            format="png",
            captureBeyondViewport=bool(capture_beyond_viewport),
            clip={
                "x": float(x),
                "y": float(y),
                "width": float(width),
                "height": float(height),
                "scale": float(scale),
            },
        )
        shot_path.write_bytes(base64.b64decode(response["data"]))
        _apply_max_dim(shot_path, max_dim=max_dim)
        if not attach:
            return str(shot_path)
        return emit_image(
            shot_path,
            label=label,
            detail=detail,
            mime_type=mime_type,
        )

    def screenshot(
        label: str = "screenshot",
        path: Any | None = None,
        full: bool = False,
        max_dim: int | None = None,
        attach: bool = True,
        detail: str = "auto",
        mime_type: str | None = None,
        timeout: float | None = None,
        timeout_s: float | None = None,
    ) -> Dict[str, Any] | str:
        """Capture and attach a screenshot in one call."""
        shot_path = capture_screenshot(
            path=path,
            full=full,
            max_dim=max_dim,
            attach=False,
            timeout=timeout,
            timeout_s=timeout_s,
        )
        if not attach:
            return shot_path
        return emit_image(
            shot_path,
            label=label,
            detail=detail,
            mime_type=mime_type,
        )

    def page_info() -> Dict[str, Any]:
        """Browser-harness page_info with a CDP-only fallback for wedged page JS."""
        original = ns.get("__browser_harness_page_info__")
        if callable(original):
            try:
                return original()
            except Exception as exc:
                fallback = _page_info_cdp_fallback(ns, exc)
                if fallback:
                    return fallback
                raise
        fallback = _page_info_cdp_fallback(ns, None)
        if fallback:
            return fallback
        raise RuntimeError("browser_harness page_info is not available")

    def _final_answer_count(data: Any) -> int | None:
        if isinstance(data, list):
            return len(data)
        if isinstance(data, dict):
            list_lengths = [len(value) for value in data.values() if isinstance(value, list)]
            if list_lengths:
                return sum(list_lengths)
        return None

    def _final_answer_preview(data: Any) -> Any:
        if isinstance(data, list):
            return data[:3]
        if isinstance(data, dict):
            preview: Dict[str, Any] = {}
            for key, value in data.items():
                preview[key] = value[:3] if isinstance(value, list) else value
            return preview
        text = str(data)
        return text[:1000] + ("..." if len(text) > 1000 else "")

    def set_final_answer(
        data: Any,
        artifact_name: str | None = None,
        mime_type: str | None = None,
    ) -> Dict[str, Any]:
        """Persist the final user-facing answer for a later `done` call.

        The full answer is written to `.final_answer.json` for the host and,
        when `artifact_name` is supplied, copied as a user-visible artifact.
        The Python `result` is only a compact readiness summary, avoiding huge
        transcript prints as the source of truth.
        """
        nonlocal artifact_dir
        if isinstance(data, str):
            result_text = data
            default_mime = "text/plain"
            default_name = "final_answer.txt"
        else:
            result_text = json.dumps(data, ensure_ascii=False, indent=2, default=str)
            default_mime = "application/json"
            default_name = "final_answer.json"
        artifact = None
        if artifact_name:
            artifact = _write_artifact(artifact_name, result_text, mime_type or default_mime)
        count = _final_answer_count(data)
        summary = {
            "ready": True,
            "count": count,
            "artifact": artifact,
            "preview": _final_answer_preview(data),
        }
        metadata = {
            "result": result_text,
            "summary": summary,
            "artifact": artifact,
        }
        (artifact_dir / ".final_answer.json").write_text(
            json.dumps(metadata, ensure_ascii=False, indent=2, default=str),
            encoding="utf-8",
        )
        ns["final_answer"] = data
        ns["final_answer_text"] = result_text
        ns["result"] = {"final_answer": summary}
        count_text = f" count={count}" if count is not None else ""
        artifact_text = f" artifact={artifact['path']}" if artifact else ""
        emit_output(f"final answer ready:{count_text}{artifact_text}")
        return summary

    def get_final_answer() -> Any:
        return ns.get("final_answer")

    def emit_browser_live_url(live_url: str) -> None:
        _record_browser_event(ns, request_id, "browser.live_url", {"live_url": str(live_url)})

    def emit_browser_state(
        url: str | None = None,
        title: str | None = None,
        status: str | None = None,
        tabs: int | None = None,
        viewport: Any | None = None,
    ) -> None:
        payload: Dict[str, Any] = {}
        if url is not None:
            payload["url"] = str(url)
        if title is not None:
            payload["title"] = str(title)
        if status is not None:
            payload["status"] = str(status)
        if tabs is not None:
            payload["tabs"] = int(tabs)
        if viewport is not None:
            payload["viewport"] = viewport
        _record_browser_event(ns, request_id, "browser.state", payload)

    def check_cancel() -> None:
        if cancel_requested:
            raise KeyboardInterrupt("cancel requested")

    def artifact_root() -> str:
        return str(artifact_dir)

    def session_metadata() -> Dict[str, Any]:
        return {
            "session_id": str(ns.get("session_id")),
            "cwd": str(ns.get("cwd")),
            "artifact_root": str(artifact_dir),
        }

    ns.update(
        {
            "emit_output": emit_output,
            "copy_artifact": copy_artifact,
            "emit_image": emit_image,
            "capture_screenshot": capture_screenshot,
            "screenshot_clip": screenshot_clip,
            "screenshot": screenshot,
            "page_info": page_info,
            "set_final_answer": set_final_answer,
            "get_final_answer": get_final_answer,
            "emit_browser_live_url": emit_browser_live_url,
            "emit_browser_state": emit_browser_state,
            "check_cancel": check_cancel,
            "artifact_root": artifact_root,
            "session_metadata": session_metadata,
        }
    )


def _page_info_cdp_fallback(ns: Dict[str, Any], error: Exception | None) -> Dict[str, Any] | None:
    helpers = ns.get("__browser_harness_helpers__")
    if helpers is None:
        return None
    payload: Dict[str, Any] = {}
    try:
        tab = helpers.current_tab()
        if isinstance(tab, dict):
            if tab.get("url"):
                payload["url"] = str(tab["url"])
            if tab.get("title"):
                payload["title"] = str(tab["title"])
    except Exception:
        pass
    try:
        metrics = helpers.cdp("Page.getLayoutMetrics")
        viewport = metrics.get("cssVisualViewport") or metrics.get("cssLayoutViewport") or {}
        layout = metrics.get("cssLayoutViewport") or {}
        content = metrics.get("cssContentSize") or {}
        width = viewport.get("clientWidth") or layout.get("clientWidth")
        height = viewport.get("clientHeight") or layout.get("clientHeight")
        page_x = viewport.get("pageX") or viewport.get("offsetX") or 0
        page_y = viewport.get("pageY") or viewport.get("offsetY") or 0
        page_width = content.get("width") or width
        page_height = content.get("height") or height
        if width is not None:
            payload["w"] = round(float(width))
        if height is not None:
            payload["h"] = round(float(height))
        payload["sx"] = round(float(page_x))
        payload["sy"] = round(float(page_y))
        if page_width is not None:
            payload["pw"] = round(float(page_width))
        if page_height is not None:
            payload["ph"] = round(float(page_height))
    except Exception:
        pass
    if error is not None:
        payload["fallback"] = "cdp"
        payload["page_info_error"] = str(error)
    return payload or None


def _auto_emit_browser_state(ns: Dict[str, Any], request_id: str) -> None:
    if not ns.get("browser_harness_available"):
        return
    helpers = ns.get("__browser_harness_helpers__")
    admin = ns.get("__browser_harness_admin__")
    if helpers is None or admin is None:
        return
    try:
        if not admin.daemon_alive():
            return
        tab = helpers.current_tab()
    except Exception:
        return
    payload: Dict[str, Any] = {}
    if isinstance(tab, dict):
        if tab.get("url"):
            payload["url"] = str(tab["url"])
        if tab.get("title"):
            payload["title"] = str(tab["title"])
        if tab.get("targetId"):
            payload["target_id"] = str(tab["targetId"])
    try:
        tabs = helpers.list_tabs(include_chrome=False)
        if isinstance(tabs, list):
            payload["tabs"] = len(tabs)
    except Exception:
        pass
    try:
        metrics = helpers.cdp("Page.getLayoutMetrics")
        viewport = metrics.get("cssVisualViewport") or metrics.get("cssLayoutViewport") or {}
        width = viewport.get("clientWidth")
        height = viewport.get("clientHeight")
        if width and height:
            payload["viewport"] = {"w": round(float(width)), "h": round(float(height))}
    except Exception:
        pass
    if not payload:
        return
    record = {"type": "browser.state", "payload": payload}
    if (ns.get("browser_events") or [])[-1:] == [record]:
        return
    _record_browser_event(ns, request_id, "browser.state", payload)


def _browser_connection_snapshot(ns: Dict[str, Any]) -> Dict[str, Any] | None:
    if not ns.get("browser_harness_available"):
        return None
    helpers = ns.get("__browser_harness_helpers__")
    admin = ns.get("__browser_harness_admin__")
    if helpers is None or admin is None:
        return None
    send = getattr(helpers, "_send", None)
    if send is None:
        return None
    try:
        if not admin.daemon_alive():
            return {"status": "disconnected"}
        response = send({"meta": "connection_status"})
    except Exception as exc:
        return {"status": "disconnected", "error": str(exc)}
    if response.get("error"):
        return {"status": "disconnected", "error": str(response.get("error"))}
    page = response.get("page") or {}
    snapshot: Dict[str, Any] = {
        "status": "connected",
        "target_id": response.get("target_id"),
        "session_id": response.get("session_id"),
    }
    if page.get("url"):
        snapshot["url"] = str(page.get("url"))
    if page.get("title"):
        snapshot["title"] = str(page.get("title"))
    return snapshot


def _emit_browser_identity_events(ns: Dict[str, Any], request_id: str) -> None:
    current = _browser_connection_snapshot(ns)
    if current is None:
        return
    previous = ns.get("__browser_identity__")
    if current.get("status") != "connected":
        if isinstance(previous, dict) and previous.get("status") == "connected":
            _record_browser_event(ns, request_id, "browser.disconnected", current)
        ns["__browser_identity__"] = current
        return

    if not isinstance(previous, dict) or previous.get("status") != "connected":
        _record_browser_event(ns, request_id, "browser.connected", current)
        ns["__browser_identity__"] = current
        return

    previous_session = previous.get("session_id")
    current_session = current.get("session_id")
    previous_target = previous.get("target_id")
    current_target = current.get("target_id")
    if previous_session and current_session and previous_session != current_session:
        _record_browser_event(
            ns,
            request_id,
            "browser.reconnected",
            {
                **current,
                "previous_session_id": previous_session,
                "stale_object_ids": True,
            },
        )
    if previous_target and current_target and previous_target != current_target:
        _record_browser_event(
            ns,
            request_id,
            "browser.target_changed",
            {
                **current,
                "previous_target_id": previous_target,
                "stale_object_ids": True,
            },
        )
    ns["__browser_identity__"] = current


def _run(request: Dict[str, Any]) -> Dict[str, Any]:
    request_id = str(request.get("id") or "")
    session_id = str(request.get("session_id") or "default")
    cwd = Path(str(request.get("cwd") or ".")).expanduser().resolve()
    artifact_dir = Path(str(request.get("artifact_dir") or cwd / "artifacts")).expanduser().resolve()
    code = str(request.get("code") or "")
    cancel_requested = bool(request.get("cancel_requested"))
    timeout_seconds = float(request.get("timeout_seconds") or 0)
    stdout = io.StringIO()
    ns: Dict[str, Any] | None = None
    old_cwd = Path.cwd()
    old_alarm_handler: Any = None
    alarm_armed = False
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stdout):
            ns = _namespace(session_id, cwd, artifact_dir)
            _install_host_helpers(ns, request_id, cancel_requested)
        cwd.mkdir(parents=True, exist_ok=True)
        os.chdir(cwd)
        if timeout_seconds > 0 and hasattr(signal, "SIGALRM"):
            old_alarm_handler = signal.getsignal(signal.SIGALRM)
            signal.signal(signal.SIGALRM, _raise_tool_timeout)
            signal.setitimer(signal.ITIMER_REAL, timeout_seconds)
            alarm_armed = True
        assert ns is not None
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stdout):
            exec(compile(code, "<browser-use-python-worker>", "exec"), ns)
        _auto_emit_browser_state(ns, request_id)
        _emit_browser_identity_events(ns, request_id)
        return {
            "id": request_id,
            "ok": True,
            "text": stdout.getvalue(),
            "error": None,
            "data": _jsonable(ns.get("result")),
            "outputs": _jsonable(ns.get("outputs") or []),
            "artifacts": _jsonable(ns.get("artifacts") or []),
            "images": _jsonable(ns.get("images") or []),
            "browser_events": _jsonable(ns.get("browser_events") or []),
            "browser_harness_available": bool(ns.get("browser_harness_available")),
            "browser_harness_error": ns.get("browser_harness_error"),
        }
    except BaseException as exc:
        if ns is not None:
            _emit_browser_identity_events(ns, request_id)
        return {
            "id": request_id,
            "ok": False,
            "text": stdout.getvalue(),
            "error": "".join(traceback.format_exception_only(type(exc), exc)).strip(),
            "data": None,
            "outputs": _jsonable((ns or {}).get("outputs") or []),
            "artifacts": _jsonable((ns or {}).get("artifacts") or []),
            "images": [],
            "browser_events": _jsonable((ns or {}).get("browser_events") or []),
            "browser_harness_available": bool((ns or {}).get("browser_harness_available")),
            "browser_harness_error": (ns or {}).get("browser_harness_error"),
        }
    finally:
        if alarm_armed:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, old_alarm_handler)
        os.chdir(old_cwd)


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
            response = _run(request)
        except BaseException as exc:
            response = {
                "id": "",
                "ok": False,
                "text": "",
                "error": "".join(traceback.format_exception_only(type(exc), exc)).strip(),
                "data": None,
                "images": [],
            }
        print(json.dumps(response, ensure_ascii=False, separators=(",", ":")), flush=True)


if __name__ == "__main__":
    main()
