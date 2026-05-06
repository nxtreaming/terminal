from __future__ import annotations

import base64
import json
import math
import os
import shutil
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple, Union
from urllib.parse import quote, urlparse

import requests

from llm_browser.browser.cdp import CdpClient, CdpConnectionError, CdpError
from llm_browser.browser.chrome import ChromeProcess, start_chrome
from llm_browser.browser.helpers import (
    decode_unserializable_js_value,
    is_internal_url,
    is_real_page_target,
    key_event_for_cdp,
    select_all_modifier,
    wrap_js_if_return_statement,
)
from llm_browser.events.event import now_ms
from llm_browser.tool.result import ToolImage


BROWSER_USE_CLOUD_API = "https://api.browser-use.com/api/v3"
DEFAULT_CDP_PORTS = (9222, 9223)
BROWSER_LEVEL_DOMAINS = {"Browser", "Target", "SystemInfo", "Schema"}

REAL_BROWSER_PROFILE_DIRS = [
    Path.home() / "Library/Application Support/Google/Chrome",
    Path.home() / "Library/Application Support/Comet",
    Path.home() / "Library/Application Support/Arc/User Data",
    Path.home() / "Library/Application Support/Dia/User Data",
    Path.home() / "Library/Application Support/Microsoft Edge",
    Path.home() / "Library/Application Support/Microsoft Edge Beta",
    Path.home() / "Library/Application Support/Microsoft Edge Dev",
    Path.home() / "Library/Application Support/Microsoft Edge Canary",
    Path.home() / "Library/Application Support/BraveSoftware/Brave-Browser",
    Path.home() / ".config/google-chrome",
    Path.home() / ".config/chromium",
    Path.home() / ".config/chromium-browser",
    Path.home() / ".config/microsoft-edge",
    Path.home() / ".config/microsoft-edge-beta",
    Path.home() / ".config/microsoft-edge-dev",
    Path.home() / ".var/app/org.chromium.Chromium/config/chromium",
    Path.home() / ".var/app/com.google.Chrome/config/google-chrome",
    Path.home() / ".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser",
    Path.home() / ".var/app/com.microsoft.Edge/config/microsoft-edge",
    Path.home() / "AppData/Local/Google/Chrome/User Data",
    Path.home() / "AppData/Local/Chromium/User Data",
    Path.home() / "AppData/Local/Microsoft/Edge/User Data",
    Path.home() / "AppData/Local/Microsoft/Edge Beta/User Data",
    Path.home() / "AppData/Local/Microsoft/Edge Dev/User Data",
    Path.home() / "AppData/Local/Microsoft/Edge SxS/User Data",
    Path.home() / "AppData/Local/BraveSoftware/Brave-Browser/User Data",
]


@dataclass(frozen=True)
class BrowserRuntimeOptions:
    mode: str = "auto"
    cdp_http_url: Optional[str] = None
    cdp_ws_url: Optional[str] = None
    chrome_path: Optional[Path] = None
    profile_template: Optional[Path] = None
    preserve_profile: bool = False
    width: int = 1280
    height: int = 900
    cloud_api_key: Optional[str] = None
    cloud_api_base: str = BROWSER_USE_CLOUD_API
    cloud_profile_id: Optional[str] = None
    cloud_profile_name: Optional[str] = None
    cloud_proxy_country: Optional[str] = None
    cloud_timeout: Optional[int] = None
    cloud_allow_resizing: Optional[bool] = None
    cloud_enable_recording: Optional[bool] = None
    cloud_custom_proxy: Optional[Dict[str, Any]] = None
    daemon_name: Optional[str] = None
    daemon_backend: Optional[str] = None

    @classmethod
    def from_env(cls) -> "BrowserRuntimeOptions":
        return cls(
            mode=os.environ.get("LLM_BROWSER_MODE") or os.environ.get("BROWSER_USE_TERMINAL_BROWSER") or "auto",
            cdp_http_url=_first_env(
                "LLM_BROWSER_CDP_HTTP_URL",
                "BROWSER_USE_CDP_HTTP_URL",
                "BU_CDP_URL",
            ),
            cdp_ws_url=_first_env(
                "LLM_BROWSER_CDP_WS_URL",
                "BROWSER_USE_CDP_WS_URL",
                "BU_CDP_WS",
            ),
            chrome_path=_env_path("LLM_BROWSER_CHROME_PATH") or _env_path("LLM_BROWSER_CHROME"),
            profile_template=_env_path("LLM_BROWSER_PROFILE_TEMPLATE"),
            preserve_profile=_env_bool("LLM_BROWSER_KEEP_CHROME_PROFILE", False),
            width=_env_int("LLM_BROWSER_WIDTH", 1280),
            height=_env_int("LLM_BROWSER_HEIGHT", 900),
            cloud_api_key=_first_env("BROWSER_USE_API_KEY", "BU_API_KEY"),
            cloud_api_base=_first_env("LLM_BROWSER_CLOUD_API_BASE", "BROWSER_USE_CLOUD_API_BASE") or BROWSER_USE_CLOUD_API,
            cloud_profile_id=_first_env("LLM_BROWSER_CLOUD_PROFILE_ID", "BROWSER_USE_CLOUD_PROFILE_ID"),
            cloud_profile_name=_first_env("LLM_BROWSER_CLOUD_PROFILE_NAME", "BROWSER_USE_CLOUD_PROFILE_NAME"),
            cloud_proxy_country=_env_optional_value("LLM_BROWSER_CLOUD_PROXY_COUNTRY"),
            cloud_timeout=_env_int_optional("LLM_BROWSER_CLOUD_TIMEOUT"),
            cloud_allow_resizing=_env_bool_optional("LLM_BROWSER_CLOUD_ALLOW_RESIZING"),
            cloud_enable_recording=_env_bool_optional("LLM_BROWSER_CLOUD_ENABLE_RECORDING"),
            cloud_custom_proxy=_env_json_object("LLM_BROWSER_CLOUD_CUSTOM_PROXY_JSON"),
            daemon_name=_first_env("LLM_BROWSER_DAEMON_NAME", "BROWSER_USE_TERMINAL_DAEMON_NAME"),
            daemon_backend=_first_env("LLM_BROWSER_DAEMON_BACKEND", "BROWSER_USE_TERMINAL_DAEMON_BACKEND"),
        )

    def normalized_mode(self) -> str:
        mode = self.mode.strip().lower().replace("_", "-")
        aliases = {
            "": "auto",
            "local": "chromium",
            "chrome": "chromium",
            "owned": "chromium",
            "owned-chrome": "chromium",
            "owned-chromium": "chromium",
            "headless": "headless-chromium",
            "headless-chrome": "headless-chromium",
            "remote": "cloud",
            "browser-use": "cloud",
            "browser-use-cloud": "cloud",
            "browser-daemon": "daemon",
            "cdp-daemon": "daemon",
            "real-chrome": "real",
            "existing": "real",
            "attach": "cdp",
        }
        return aliases.get(mode, mode)

    def cloud_create_body(self) -> Dict[str, Any]:
        body: Dict[str, Any] = {}
        if self.cloud_profile_id:
            body["profileId"] = self.cloud_profile_id
        if self.cloud_proxy_country is not None:
            proxy = self.cloud_proxy_country.strip()
            body["proxyCountryCode"] = None if proxy.lower() in {"", "none", "null", "off", "false"} else proxy
        if self.cloud_timeout is not None:
            body["timeout"] = self.cloud_timeout
        if self.width:
            body["browserScreenWidth"] = self.width
        if self.height:
            body["browserScreenHeight"] = self.height
        if self.cloud_allow_resizing is not None:
            body["allowResizing"] = self.cloud_allow_resizing
        if self.cloud_enable_recording is not None:
            body["enableRecording"] = self.cloud_enable_recording
        if self.cloud_custom_proxy is not None:
            body["customProxy"] = self.cloud_custom_proxy
        return body


@dataclass(frozen=True)
class DiscoveredCdpEndpoint:
    http_url: Optional[str] = None
    websocket_url: Optional[str] = None
    source: str = ""


class BrowserRuntime:
    def __init__(self, root_dir: Path, http_url: Optional[str] = None, preserve_profile: bool = False) -> None:
        self.root_dir = root_dir
        self.http_url = http_url
        self.preserve_profile = preserve_profile
        self.mode = "unknown"
        self.chrome: Optional[ChromeProcess] = None
        self.client: Optional[CdpClient] = None
        self.target: Optional[Dict[str, Any]] = None
        self.websocket_url: Optional[str] = None
        self.default_session_id: Optional[str] = None
        self.browser_level_ws = False
        self.cloud_browser_id: Optional[str] = None
        self.cloud_live_url: Optional[str] = None
        self.cloud_api_key: Optional[str] = None
        self.cloud_api_base: str = BROWSER_USE_CLOUD_API
        self._screenshot_index = 0
        self._trace_index = 0
        self._event_log: List[Dict[str, Any]] = []
        self.pending_dialog: Optional[Dict[str, Any]] = None
        self.downloads_dir = root_dir / "downloads"
        self.downloads_dir.mkdir(parents=True, exist_ok=True)

    @classmethod
    def start(
        cls,
        root_dir: Path,
        headless: bool = False,
        options: Optional[BrowserRuntimeOptions] = None,
    ) -> "BrowserRuntime":
        options = options or BrowserRuntimeOptions.from_env()
        mode = options.normalized_mode()
        if mode == "headless-chromium":
            headless = True
            mode = "chromium"

        if mode == "auto":
            if options.cdp_http_url:
                return cls.attach_devtools_http(root_dir=root_dir, http_url=options.cdp_http_url, mode="cdp")
            if options.cdp_ws_url:
                return cls.attach_ws(root_dir=root_dir, websocket_url=options.cdp_ws_url, mode="cdp")
            mode = "chromium"

        if mode == "cdp":
            if options.cdp_http_url:
                return cls.attach_devtools_http(root_dir=root_dir, http_url=options.cdp_http_url, mode="cdp")
            if options.cdp_ws_url:
                return cls.attach_ws(root_dir=root_dir, websocket_url=options.cdp_ws_url, mode="cdp")
            raise RuntimeError("browser mode 'cdp' requires --cdp-url, --cdp-ws, LLM_BROWSER_CDP_HTTP_URL, or LLM_BROWSER_CDP_WS_URL")

        if mode == "real":
            return cls.attach_real(root_dir=root_dir, options=options)

        if mode == "cloud":
            return cls.start_cloud(root_dir=root_dir, options=options)

        if mode == "daemon":
            from llm_browser.browser.daemon_client import DaemonBrowserRuntime

            return DaemonBrowserRuntime.start(root_dir=root_dir, headless=headless, options=options)  # type: ignore[return-value]

        if mode not in {"chromium"}:
            raise RuntimeError(f"unknown browser mode {options.mode!r}; expected auto, chromium, real, cdp, cloud, or daemon")

        runtime = cls(root_dir=root_dir, preserve_profile=options.preserve_profile)
        runtime.mode = "chromium"
        try:
            runtime.chrome = start_chrome(
                root_dir=root_dir,
                profile_template=options.profile_template,
                chrome_path=options.chrome_path,
                headless=headless,
                width=options.width,
                height=options.height,
            )
            runtime.downloads_dir = runtime.chrome.config.downloads_dir
            runtime.http_url = runtime.chrome.http_url
            runtime.attach_first_page()
        except BaseException:
            runtime.close()
            raise
        return runtime

    @classmethod
    def attach(cls, root_dir: Path, http_url: str) -> "BrowserRuntime":
        runtime = cls(root_dir=root_dir, http_url=http_url.rstrip("/"), preserve_profile=True)
        runtime.mode = "cdp"
        runtime.attach_first_page()
        return runtime

    @classmethod
    def attach_devtools_http(cls, root_dir: Path, http_url: str, mode: str = "cdp") -> "BrowserRuntime":
        try:
            runtime = cls.attach(root_dir=root_dir, http_url=http_url)
            if isinstance(runtime, cls):
                runtime.mode = mode
            return runtime
        except Exception:
            websocket_url = _ws_from_devtools_active_port(http_url)
            if websocket_url:
                return cls.attach_ws(root_dir=root_dir, websocket_url=websocket_url, mode=mode)
            raise

    @classmethod
    def attach_real(cls, root_dir: Path, options: Optional[BrowserRuntimeOptions] = None) -> "BrowserRuntime":
        options = options or BrowserRuntimeOptions.from_env()
        if options.cdp_http_url:
            return cls.attach_devtools_http(root_dir=root_dir, http_url=options.cdp_http_url, mode="real")
        if options.cdp_ws_url:
            return cls.attach_ws(root_dir=root_dir, websocket_url=options.cdp_ws_url, mode="real")
        endpoint = discover_real_browser_endpoint()
        if endpoint.http_url:
            return cls.attach_devtools_http(root_dir=root_dir, http_url=endpoint.http_url, mode="real")
        if endpoint.websocket_url:
            return cls.attach_ws(root_dir=root_dir, websocket_url=endpoint.websocket_url, mode="real")
        raise RuntimeError("real browser discovery returned no CDP endpoint")

    @classmethod
    def start_cloud(cls, root_dir: Path, options: Optional[BrowserRuntimeOptions] = None) -> "BrowserRuntime":
        options = options or BrowserRuntimeOptions.from_env()
        if not options.cloud_api_key:
            raise RuntimeError("Browser Use cloud mode requires BROWSER_USE_API_KEY or BU_API_KEY")
        browser_id: Optional[str] = None
        try:
            body = options.cloud_create_body()
            if options.cloud_profile_name:
                if options.cloud_profile_id:
                    raise RuntimeError("pass cloud profile id or cloud profile name, not both")
                body["profileId"] = _resolve_cloud_profile_name(
                    api_base=options.cloud_api_base,
                    api_key=options.cloud_api_key,
                    profile_name=options.cloud_profile_name,
                )
            browser = _browser_use_request(
                api_base=options.cloud_api_base,
                api_key=options.cloud_api_key,
                path="/browsers",
                method="POST",
                body=body,
            )
            browser_id = str(browser.get("id") or "")
            websocket_url = _cloud_browser_websocket_url(browser)
            runtime = cls.attach_ws(root_dir=root_dir, websocket_url=websocket_url, mode="cloud")
            runtime.cloud_browser_id = browser_id or None
            runtime.cloud_live_url = str(browser.get("liveUrl") or browser.get("live_url") or "") or None
            runtime.cloud_api_key = options.cloud_api_key
            runtime.cloud_api_base = options.cloud_api_base
            runtime.preserve_profile = True
            return runtime
        except BaseException:
            if browser_id and options.cloud_api_key:
                _stop_cloud_browser(options.cloud_api_base, options.cloud_api_key, browser_id)
            raise

    @classmethod
    def attach_ws(cls, root_dir: Path, websocket_url: str, mode: str = "cdp") -> "BrowserRuntime":
        runtime = cls(root_dir=root_dir, preserve_profile=True)
        runtime.mode = mode
        runtime.websocket_url = websocket_url
        runtime.client = CdpClient(websocket_url)
        runtime.client.connect()
        runtime._initialize_websocket_target()
        return runtime

    def close(self) -> None:
        if self.client is not None:
            self.client.close()
            self.client = None
        if self.chrome is not None:
            profile_dir = self.chrome.config.user_data_dir
            self.chrome.stop()
            self.chrome = None
            if not self.preserve_profile:
                shutil.rmtree(profile_dir, ignore_errors=True)
        if self.cloud_browser_id and self.cloud_api_key:
            _stop_cloud_browser(self.cloud_api_base, self.cloud_api_key, self.cloud_browser_id)
            self.cloud_browser_id = None

    def connection_info(self) -> Dict[str, Any]:
        return {
            "mode": self.mode,
            "http_url": self.http_url,
            "websocket_url": _redact_url(self.websocket_url),
            "browser_level_ws": self.browser_level_ws,
            "target": self.target,
            "cloud_browser_id": self.cloud_browser_id,
            "cloud_live_url": self.cloud_live_url,
            "downloads_dir": str(self.downloads_dir),
        }

    def version(self) -> Dict[str, Any]:
        if not self.http_url:
            return self.cdp("Browser.getVersion")
        return _get_json(f"{self.http_url}/json/version", timeout=5)

    def targets(self) -> List[Dict[str, Any]]:
        if not self.http_url:
            if self.client is not None and self.browser_level_ws:
                result = self.client.call("Target.getTargets", timeout_s=5)
                return [_normalize_target_info(target) for target in result.get("targetInfos", [])]
            return [self.target or {"id": "external", "type": "page", "url": ""}]
        return _get_json(f"{self.http_url}/json/list", timeout=5)

    def tabs(self) -> List[Dict[str, Any]]:
        return [target for target in self.targets() if target.get("type") == "page"]

    def list_tabs(self, include_internal: bool = True) -> List[Dict[str, Any]]:
        pages = self.tabs()
        if include_internal:
            return pages
        return [target for target in pages if is_real_page_target(target)]

    def attach_first_page(self) -> Dict[str, Any]:
        pages = self.tabs()
        real_pages = [page for page in pages if is_real_page_target(page)]
        if real_pages:
            return self.attach_target(real_pages[0])
        return self.new_tab("about:blank")

    def attach_tab(
        self,
        target_id: Optional[str] = None,
        index: Optional[int] = None,
        url_contains: Optional[str] = None,
    ) -> Dict[str, Any]:
        pages = self.tabs()
        if target_id is not None:
            for page in pages:
                if page.get("id") == target_id:
                    return self.attach_target(page)
            raise ValueError(f"page target not found: {target_id}")
        if url_contains is not None:
            for page in pages:
                if url_contains in str(page.get("url") or ""):
                    return self.attach_target(page)
            raise ValueError(f"page URL containing {url_contains!r} not found")
        if index is None:
            index = 0
        if index < 0 or index >= len(pages):
            raise IndexError(f"page index {index} out of range for {len(pages)} page(s)")
        return self.attach_target(pages[index])

    def attach_target(self, target: Dict[str, Any]) -> Dict[str, Any]:
        if self.browser_level_ws and self.client is not None and not target.get("webSocketDebuggerUrl"):
            return self._attach_browser_target(str(target.get("id") or target.get("targetId") or ""))

        websocket_url = target.get("webSocketDebuggerUrl")
        if not websocket_url:
            raise RuntimeError(f"target has no websocket URL: {target}")
        if self.client is not None:
            self.client.close()
        self.target = target
        self.websocket_url = websocket_url
        self.default_session_id = None
        self.browser_level_ws = False
        self.client = CdpClient(websocket_url)
        self.client.connect()
        self._enable_page_domains()
        return target

    def switch_tab(self, target: Union[Dict[str, Any], str]) -> Dict[str, Any]:
        target_id = str(target.get("id") or target.get("targetId") if isinstance(target, dict) else target)
        if not target_id:
            raise RuntimeError("switch_tab requires a target id or target dict")
        if self.client is not None and self.browser_level_ws:
            try:
                self.client.call("Target.activateTarget", {"targetId": target_id}, timeout_s=5)
            except Exception:
                pass
            return self._attach_browser_target(target_id)
        for page in self.tabs():
            if str(page.get("id") or page.get("targetId") or "") == target_id:
                try:
                    self.cdp("Target.activateTarget", {"targetId": target_id}, retry=False)
                except Exception:
                    pass
                return self.attach_target(page)
        raise ValueError(f"page target not found: {target_id}")

    def current_tab(self) -> Dict[str, Any]:
        target_id = str((self.target or {}).get("id") or (self.target or {}).get("targetId") or self.target_id_from_session() or "")
        if target_id:
            found = self._target_info_by_id(target_id)
            if found:
                return found
        return dict(self.target or {"id": "external", "type": "page", "url": ""})

    def target_id_from_session(self) -> Optional[str]:
        return str((self.target or {}).get("id") or (self.target or {}).get("targetId") or "") or None

    def ensure_real_tab(self) -> Optional[Dict[str, Any]]:
        try:
            current = self.current_tab()
            if is_real_page_target(current):
                return current
        except Exception:
            pass
        real_tabs = self.list_tabs(include_internal=False)
        if not real_tabs:
            return None
        return self.switch_tab(real_tabs[0])

    def iframe_target(self, url_substr: str) -> Optional[str]:
        for target in self.targets():
            if target.get("type") == "iframe" and url_substr in str(target.get("url") or ""):
                return str(target.get("id") or target.get("targetId") or "")
        return None

    def new_tab(self, url: str = "about:blank") -> Dict[str, Any]:
        if not self.http_url and self.browser_level_ws and self.client is not None:
            result = self.client.call("Target.createTarget", {"url": "about:blank"})
            target = self._attach_browser_target(str(result["targetId"]))
            if url != "about:blank":
                self.navigate(url, wait=False)
            return target
        if not self.http_url:
            if url != "about:blank":
                self.navigate(url, wait=False)
            return self.target or {"id": "external", "type": "page", "url": url}
        create_url = "about:blank" if url != "about:blank" else url
        encoded = quote(create_url, safe=":/?&=%#")
        response = requests.put(f"{self.http_url}/json/new?{encoded}", timeout=5)
        if response.status_code >= 400:
            response = requests.get(f"{self.http_url}/json/new?{encoded}", timeout=5)
        response.raise_for_status()
        target = self.attach_target(response.json())
        target_url = str(target.get("url") or "")
        if url != "about:blank" and target_url in {"", "about:blank"}:
            self.navigate(url, wait=False)
        return target

    def cdp(
        self,
        method: str,
        params: Optional[Dict[str, Any]] = None,
        session_id: Optional[str] = None,
        timeout_s: Optional[float] = None,
        retry: bool = True,
    ) -> Dict[str, Any]:
        if self.client is None:
            self.attach_first_page()
        assert self.client is not None
        effective_session_id = session_id if session_id is not None else self._default_session_id_for_method(method)
        try:
            return self.client.call(method, params=params, session_id=effective_session_id, timeout_s=timeout_s)
        except CdpConnectionError:
            if not retry:
                raise
            self._reattach_after_disconnect()
            assert self.client is not None
            effective_session_id = session_id if session_id is not None else self._default_session_id_for_method(method)
            return self.client.call(method, params=params, session_id=effective_session_id, timeout_s=timeout_s)
        except CdpError as exc:
            if (
                retry
                and effective_session_id is not None
                and effective_session_id == self.default_session_id
                and "Session with given id not found" in str(exc)
            ):
                target_id = str((self.target or {}).get("id") or (self.target or {}).get("targetId") or "")
                if target_id:
                    self._attach_browser_target(target_id)
                    effective_session_id = session_id if session_id is not None else self._default_session_id_for_method(method)
                    return self.client.call(method, params=params, session_id=effective_session_id, timeout_s=timeout_s)
            raise

    def _reattach_after_disconnect(self) -> None:
        if self.client is not None:
            self.client.close()
            self.client = None
        target_id = str((self.target or {}).get("id") or "")
        if self.http_url and target_id:
            for page in self.tabs():
                if page.get("id") == target_id:
                    self.attach_target(page)
                    return
        if self.websocket_url:
            self.client = CdpClient(self.websocket_url)
            self.client.connect()
            if self.browser_level_ws:
                if target_id:
                    self._attach_browser_target(target_id)
                else:
                    self._initialize_websocket_target()
            else:
                self._enable_page_domains()
            return
        self.attach_first_page()

    def _initialize_websocket_target(self) -> None:
        assert self.client is not None
        self.target = {"id": "external", "type": "page", "url": "", "webSocketDebuggerUrl": self.websocket_url or ""}
        self.default_session_id = None
        self.browser_level_ws = False
        try:
            result = self.client.call("Target.getTargets", timeout_s=2)
        except CdpError:
            self._enable_page_domains()
            return
        except CdpConnectionError:
            raise
        except Exception:
            self._enable_page_domains()
            return
        raw_targets = result.get("targetInfos")
        if not isinstance(raw_targets, list):
            self._enable_page_domains()
            return
        targets = [_normalize_target_info(target) for target in raw_targets]
        self.browser_level_ws = True
        pages = [target for target in targets if target.get("type") == "page"]
        real_pages = [target for target in pages if is_real_page_target(target)]
        target = real_pages[0] if real_pages else (pages[0] if pages else self._create_browser_target("about:blank"))
        self._attach_browser_target(str(target.get("id") or target.get("targetId") or ""))

    def _create_browser_target(self, url: str) -> Dict[str, Any]:
        assert self.client is not None
        result = self.client.call("Target.createTarget", {"url": url})
        return {"id": str(result["targetId"]), "type": "page", "url": url}

    def _attach_browser_target(self, target_id: str) -> Dict[str, Any]:
        if not target_id:
            raise RuntimeError("browser-level CDP attach requires a target id")
        assert self.client is not None
        old_session_id = self.default_session_id
        try:
            self.client.call("Target.activateTarget", {"targetId": target_id}, timeout_s=5)
        except Exception:
            pass
        result = self.client.call("Target.attachToTarget", {"targetId": target_id, "flatten": True})
        self.default_session_id = str(result["sessionId"])
        self.browser_level_ws = True
        target = self._target_info_by_id(target_id) or {"id": target_id, "type": "page", "url": ""}
        self.target = target
        if old_session_id and old_session_id != self.default_session_id:
            try:
                self.client.call("Network.disable", session_id=old_session_id, timeout_s=2)
            except Exception:
                pass
        self._enable_page_domains()
        return target

    def _target_info_by_id(self, target_id: str) -> Optional[Dict[str, Any]]:
        if self.client is None:
            return None
        try:
            result = self.client.call("Target.getTargets", timeout_s=5)
        except Exception:
            return None
        for target in result.get("targetInfos", []):
            normalized = _normalize_target_info(target)
            if normalized.get("id") == target_id:
                return normalized
        return None

    def _enable_page_domains(self) -> None:
        for domain in ("Page", "DOM", "Runtime", "Network", "Log"):
            try:
                self.cdp(f"{domain}.enable", retry=False)
            except Exception:
                pass
        self._configure_downloads()

    def _configure_downloads(self) -> None:
        self.downloads_dir.mkdir(parents=True, exist_ok=True)
        attempts = (
            ("Browser.setDownloadBehavior", {"behavior": "allow", "downloadPath": str(self.downloads_dir), "eventsEnabled": True}),
            ("Page.setDownloadBehavior", {"behavior": "allow", "downloadPath": str(self.downloads_dir)}),
        )
        for method, params in attempts:
            try:
                self.cdp(method, params, retry=False)
                return
            except Exception:
                continue

    def _default_session_id_for_method(self, method: str) -> Optional[str]:
        if not self.default_session_id:
            return None
        domain = method.split(".", 1)[0]
        if domain in BROWSER_LEVEL_DOMAINS:
            return None
        return self.default_session_id

    def navigate(self, url: str, wait: bool = True, timeout_s: float = 20.0) -> Dict[str, Any]:
        result = self.cdp("Page.navigate", {"url": url})
        if wait:
            self.wait_for_load(timeout_s=timeout_s)
        return result

    def js(
        self,
        expression: str,
        await_promise: bool = False,
        repl_mode: Optional[bool] = None,
        user_gesture: bool = False,
    ) -> Any:
        expression = wrap_js_if_return_statement(expression)
        effective_repl_mode = self._default_repl_mode(expression, await_promise) if repl_mode is None else repl_mode
        response = self.cdp(
            "Runtime.evaluate",
            {
                "expression": expression,
                "returnByValue": True,
                "awaitPromise": await_promise,
                "replMode": effective_repl_mode,
                "userGesture": user_gesture,
            },
        )
        result = response.get("result", {})
        details = response.get("exceptionDetails")
        if details or result.get("subtype") == "error":
            raise RuntimeError(f"JavaScript evaluation failed: {details or result}")
        if "value" in result:
            return result["value"]
        if "unserializableValue" in result:
            return decode_unserializable_js_value(str(result["unserializableValue"]))
        return None

    def _default_repl_mode(self, expression: str, await_promise: bool) -> bool:
        if not await_promise:
            return True
        snippet = expression.lstrip()
        if snippet.startswith("(async") or snippet.startswith("async "):
            return False
        promise_markers = ("fetch(", ".then(", "Promise.", "new Promise")
        return not any(marker in snippet for marker in promise_markers)

    def wait_for_load(self, timeout_s: float = 20.0) -> None:
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            try:
                state = self.js("document.readyState")
                if state in {"interactive", "complete"}:
                    return
            except Exception:
                pass
            time.sleep(0.1)
        raise TimeoutError("page did not reach interactive/complete readyState")

    def wait_until(self, expression: str, timeout_s: float = 20.0, interval_s: float = 0.25) -> Any:
        deadline = time.time() + timeout_s
        last_error: Optional[Exception] = None
        last_value: Any = None
        while time.time() < deadline:
            try:
                last_value = self.js(expression)
                if last_value:
                    return last_value
            except Exception as exc:
                last_error = exc
            time.sleep(interval_s)
        if last_error is not None:
            raise TimeoutError(f"condition did not become truthy before timeout: {last_error}") from last_error
        raise TimeoutError(f"condition did not become truthy before timeout; last value: {last_value!r}")

    def wait_for_selector(self, selector: str, timeout_s: float = 20.0, visible: bool = False) -> Any:
        expression = json.dumps(selector)
        if visible:
            return self.wait_until(
                "(() => {"
                f"const el = document.querySelector({expression});"
                "if (!el) return false;"
                "const rect = el.getBoundingClientRect();"
                "return !!(rect.width || rect.height || el.getClientRects().length);"
                "})()",
                timeout_s=timeout_s,
            )
        return self.wait_until(f"document.querySelector({expression}) !== null", timeout_s=timeout_s)

    def wait_for_text(self, text: str, timeout_s: float = 20.0) -> Any:
        needle = json.dumps(text)
        return self.wait_until(
            f"((document.body && document.body.innerText) || '').includes({needle})",
            timeout_s=timeout_s,
        )

    def page_info(self) -> Dict[str, Any]:
        dialog = self.pending_dialog_info()
        if dialog:
            return {"dialog": dialog}
        raw = self.js(
            """
            (() => {
              const de = document.documentElement || {};
              const body = document.body || {};
              const pageWidth = de.scrollWidth || body.scrollWidth || innerWidth || 0;
              const pageHeight = de.scrollHeight || body.scrollHeight || innerHeight || 0;
              return JSON.stringify({
                url: location.href || '',
                title: document.title || '',
                w: innerWidth || 0,
                h: innerHeight || 0,
                sx: scrollX || 0,
                sy: scrollY || 0,
                pw: pageWidth,
                ph: pageHeight
              });
            })()
            """
        )
        return json.loads(raw or "{}")

    def visible_text(self, max_chars: int = 8000) -> str:
        text = self.js(
            "(() => document.body ? document.body.innerText : '')()",
            await_promise=False,
        )
        return str(text or "")[:max_chars]

    def links(self, limit: int = 200) -> List[Dict[str, str]]:
        raw = self.js(
            "JSON.stringify(Array.from(document.links).slice(0, arguments_limit).map(a => "
            "({text:(a.innerText||a.textContent||'').trim(), href:a.href, title:a.title||''})))".replace(
                "arguments_limit", str(int(limit))
            )
        )
        return json.loads(raw or "[]")

    def screenshot(
        self,
        label: str = "screenshot",
        attach: bool = True,
        full_page: bool = False,
        timeout_s: float = 8.0,
        clip: Optional[Dict[str, float]] = None,
    ) -> ToolImage:
        params: Dict[str, Any] = {"format": "png", "fromSurface": True}
        if clip is not None:
            params["captureBeyondViewport"] = True
            params["clip"] = {
                "x": max(0.0, float(clip.get("x") or 0)),
                "y": max(0.0, float(clip.get("y") or 0)),
                "width": max(1.0, float(clip.get("width") or 1)),
                "height": max(1.0, float(clip.get("height") or 1)),
                "scale": float(clip.get("scale") or 1),
            }
        elif full_page:
            params["captureBeyondViewport"] = True
            metrics = self.cdp("Page.getLayoutMetrics", timeout_s=timeout_s, retry=False)
            size = metrics.get("cssContentSize") or metrics.get("contentSize") or {}
            width = max(1, int(math.ceil(float(size.get("width") or 1280))))
            height = max(1, int(math.ceil(float(size.get("height") or 900))))
            params["clip"] = {"x": 0, "y": 0, "width": width, "height": height, "scale": 1}
        result = self.cdp("Page.captureScreenshot", params, timeout_s=timeout_s, retry=False)
        data = base64.b64decode(result["data"])

        self._screenshot_index += 1
        safe_label = "".join(ch if ch.isalnum() or ch in {"-", "_"} else "_" for ch in label).strip("_")
        if not safe_label:
            safe_label = "screenshot"
        path = self.root_dir / "screenshots" / f"{self._screenshot_index:03d}_{safe_label}.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

        info = self._safe_page_info()
        image = ToolImage(
            label=label,
            path=str(path),
            order=self._screenshot_index,
            ts_ms=now_ms(),
            url=str(info.get("url", "")),
            title=str(info.get("title", "")),
        )
        path.with_suffix(".json").write_text(json.dumps(image.to_dict(), indent=2) + "\n", encoding="utf-8")
        return image

    def _safe_page_info(self) -> Dict[str, Any]:
        try:
            return self.page_info()
        except Exception:
            return {
                "url": str((self.target or {}).get("url") or ""),
                "title": str((self.target or {}).get("title") or ""),
                "w": 0,
                "h": 0,
                "sx": 0,
                "sy": 0,
                "pw": 0,
                "ph": 0,
            }

    def click_at(self, x: float, y: float, button: str = "left", clicks: int = 1) -> None:
        if _env_bool("LLM_BROWSER_DEBUG_CLICKS", False):
            self._save_debug_click(x, y)
        base = {"x": x, "y": y, "button": button, "clickCount": clicks}
        self.cdp("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x, "y": y})
        self.cdp("Input.dispatchMouseEvent", {"type": "mousePressed", **base})
        self.cdp("Input.dispatchMouseEvent", {"type": "mouseReleased", **base})

    def type_text(self, text: str) -> None:
        self.cdp("Input.insertText", {"text": text})

    def press(self, key: str) -> None:
        self.press_key(key)

    def press_key(self, key: str, modifiers: int = 0) -> None:
        event, text = key_event_for_cdp(key, modifiers=modifiers)
        self.cdp("Input.dispatchKeyEvent", {"type": "keyDown", **event})
        if text and len(text) == 1:
            char_event = {k: v for k, v in event.items() if k != "text"}
            self.cdp("Input.dispatchKeyEvent", {"type": "char", "text": text, **char_event})
        self.cdp("Input.dispatchKeyEvent", {"type": "keyUp", **event})

    def fill_input(self, selector: str, text: str, clear_first: bool = True, timeout_s: float = 0.0) -> None:
        if timeout_s > 0:
            self.wait_for_selector(selector, timeout_s=timeout_s, visible=False)
        selector_json = json.dumps(selector)
        focused = self.js(
            f"(()=>{{const e=document.querySelector({selector_json});"
            "if(!e)return false;e.focus();return true;}})()"
        )
        if not focused:
            raise RuntimeError(f"fill_input: element not found: {selector!r}")
        if clear_first:
            select_all = {
                "key": "a",
                "code": "KeyA",
                "modifiers": select_all_modifier(),
                "windowsVirtualKeyCode": 65,
                "nativeVirtualKeyCode": 65,
            }
            self.cdp("Input.dispatchKeyEvent", {"type": "rawKeyDown", **select_all})
            self.cdp("Input.dispatchKeyEvent", {"type": "keyUp", **select_all})
            self.press_key("Backspace")
        for char in text:
            self.press_key(char)
        self.js(
            f"(()=>{{const e=document.querySelector({selector_json});"
            "if(!e)return;"
            "e.dispatchEvent(new Event('input',{bubbles:true}));"
            "e.dispatchEvent(new Event('change',{bubbles:true}));}})()"
        )

    def scroll(self, dx: float = 0, dy: float = 500, x: float = 500, y: float = 500) -> None:
        self.cdp("Input.dispatchMouseEvent", {"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy})

    def drain_events(self, timeout_s: float = 0.05, max_events: int = 1000) -> List[Dict[str, Any]]:
        if self.client is None:
            return []
        try:
            raw_events = self.client.drain_events(timeout_s=timeout_s, max_events=max_events)
        except TypeError:
            raw_events = self.client.drain_events()
        events = [event for event in raw_events if isinstance(event, dict)]
        return self._record_events(events)

    def pending_dialog_info(self, drain: bool = True) -> Optional[Dict[str, Any]]:
        if drain:
            self.drain_events(timeout_s=0.02)
        return dict(self.pending_dialog) if self.pending_dialog else None

    def recent_cdp_events(
        self,
        prefix: Optional[str] = None,
        limit: int = 100,
        drain: bool = True,
        timeout_s: float = 0.02,
    ) -> List[Dict[str, Any]]:
        if drain:
            self.drain_events(timeout_s=timeout_s)
        events = self._event_log
        if prefix:
            events = [event for event in events if str(event.get("method") or "").startswith(prefix)]
        return events[-max(0, int(limit)) :]

    def recent_console_events(self, limit: int = 50, drain: bool = True) -> List[Dict[str, Any]]:
        events = self.recent_cdp_events(limit=limit * 4, drain=drain)
        console = []
        for event in events:
            method = str(event.get("method") or "")
            if method not in {"Runtime.consoleAPICalled", "Log.entryAdded"}:
                continue
            params = event.get("params") if isinstance(event.get("params"), dict) else {}
            if method == "Runtime.consoleAPICalled":
                args = params.get("args") if isinstance(params.get("args"), list) else []
                text = " ".join(_remote_object_text(arg) for arg in args if isinstance(arg, dict)).strip()
                console.append(
                    {
                        "method": method,
                        "type": params.get("type"),
                        "text": text,
                        "timestamp": params.get("timestamp"),
                        "raw": event,
                    }
                )
            else:
                entry = params.get("entry") if isinstance(params.get("entry"), dict) else {}
                console.append(
                    {
                        "method": method,
                        "type": entry.get("level") or entry.get("source"),
                        "text": entry.get("text"),
                        "url": entry.get("url"),
                        "lineNumber": entry.get("lineNumber"),
                        "raw": event,
                    }
                )
        return console[-max(0, int(limit)) :]

    def recent_network_events(self, limit: int = 100, drain: bool = True) -> List[Dict[str, Any]]:
        return self.recent_cdp_events(prefix="Network.", limit=limit, drain=drain)

    def recent_network_failures(self, limit: int = 50, drain: bool = True) -> List[Dict[str, Any]]:
        events = self.recent_network_events(limit=limit * 8, drain=drain)
        failures = []
        for event in events:
            method = str(event.get("method") or "")
            params = event.get("params") if isinstance(event.get("params"), dict) else {}
            if method == "Network.loadingFailed":
                failures.append(
                    {
                        "method": method,
                        "requestId": params.get("requestId"),
                        "errorText": params.get("errorText"),
                        "blockedReason": params.get("blockedReason"),
                        "canceled": params.get("canceled"),
                        "raw": event,
                    }
                )
                continue
            if method == "Network.responseReceived":
                response = params.get("response") if isinstance(params.get("response"), dict) else {}
                status = int(response.get("status") or 0)
                if status >= 400:
                    failures.append(
                        {
                            "method": method,
                            "requestId": params.get("requestId"),
                            "status": status,
                            "url": response.get("url"),
                            "statusText": response.get("statusText"),
                            "raw": event,
                        }
                    )
        return failures[-max(0, int(limit)) :]

    def wait_for_network_idle(self, timeout_s: float = 10.0, idle_ms: int = 500) -> bool:
        deadline = time.time() + timeout_s
        last_activity = time.time()
        inflight = set()
        active_session = self.default_session_id
        while time.time() < deadline:
            for event in self.drain_events(timeout_s=0.05):
                event_session = event.get("sessionId") or event.get("session_id")
                if active_session is not None and event_session not in {active_session, None}:
                    continue
                method = str(event.get("method") or "")
                params = event.get("params") if isinstance(event.get("params"), dict) else {}
                if method == "Network.requestWillBeSent":
                    inflight.add(params.get("requestId"))
                    last_activity = time.time()
                elif method in {"Network.loadingFinished", "Network.loadingFailed"}:
                    inflight.discard(params.get("requestId"))
                    last_activity = time.time()
                elif method.startswith("Network."):
                    last_activity = time.time()
            if not inflight and (time.time() - last_activity) * 1000 >= idle_ms:
                return True
            time.sleep(0.1)
        return False

    def download_info(self, limit: int = 100, drain: bool = True) -> Dict[str, Any]:
        if drain:
            self.drain_events(timeout_s=0.02)
        files: List[Dict[str, Any]] = []
        if self.downloads_dir.exists():
            for path in self.downloads_dir.rglob("*"):
                if not path.is_file():
                    continue
                try:
                    stat = path.stat()
                except OSError:
                    continue
                files.append(
                    {
                        "name": path.name,
                        "path": str(path),
                        "relative_path": str(path.relative_to(self.downloads_dir)),
                        "size": stat.st_size,
                        "mtime": stat.st_mtime,
                        "complete": not path.name.endswith(".crdownload"),
                    }
                )
        files.sort(key=lambda item: float(item["mtime"]), reverse=True)
        events = [
            event
            for event in self._event_log
            if str(event.get("method") or "").startswith(("Browser.download", "Page.download"))
        ][-max(0, int(limit)) :]
        return {
            "downloads_dir": str(self.downloads_dir),
            "mode": self.mode,
            "files": files[: max(0, int(limit))],
            "events": events,
        }

    def save_browser_trace(self, label: str = "browser_trace", include_history: bool = True) -> Dict[str, Any]:
        drained = self.drain_events(timeout_s=0.05)
        events = self._event_log if include_history else drained
        self._trace_index += 1
        safe_label = "".join(ch if ch.isalnum() or ch in {"-", "_"} else "_" for ch in label).strip("_")
        if not safe_label:
            safe_label = "browser_trace"
        path = self.root_dir / "traces" / f"{self._trace_index:03d}_{safe_label}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "ts_ms": now_ms(),
            "connection": self.connection_info(),
            "event_count": len(events),
            "drained_count": len(drained),
            "events": events,
        }
        path.write_text(json.dumps(payload, indent=2, default=str) + "\n", encoding="utf-8")
        return {"path": str(path), "event_count": len(events), "drained_count": len(drained)}

    def _record_events(self, events: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        stamped = []
        ts_ms = now_ms()
        for event in events:
            item = dict(event)
            item.setdefault("_ts_ms", ts_ms)
            self._track_event_state(item)
            stamped.append(item)
        if stamped:
            self._event_log.extend(stamped)
            if len(self._event_log) > 5000:
                self._event_log = self._event_log[-5000:]
        return stamped

    def _track_event_state(self, event: Dict[str, Any]) -> None:
        method = str(event.get("method") or "")
        params = event.get("params") if isinstance(event.get("params"), dict) else {}
        if method == "Page.javascriptDialogOpening":
            self.pending_dialog = dict(params)
        elif method == "Page.javascriptDialogClosed":
            self.pending_dialog = None

    def _save_debug_click(self, x: float, y: float) -> None:
        try:
            from PIL import Image, ImageDraw

            image = self.screenshot(f"debug_click_{self._screenshot_index + 1}", attach=False)
            path = Path(image.path)
            device_pixel_ratio = float(self.js("window.devicePixelRatio") or 1)
            with Image.open(path) as img:
                draw = ImageDraw.Draw(img)
                px, py = int(x * device_pixel_ratio), int(y * device_pixel_ratio)
                radius = int(15 * device_pixel_ratio)
                width = max(1, int(3 * device_pixel_ratio))
                draw.ellipse([px - radius, py - radius, px + radius, py + radius], outline="red", width=width)
                draw.line([px - radius, py, px + radius, py], fill="red", width=width)
                draw.line([px, py - radius, px, py + radius], fill="red", width=width)
                img.save(path)
        except Exception:
            pass


def _key_event(key: str) -> Dict[str, Any]:
    common = {
        "Enter": ("Enter", "Enter", 13),
        "Escape": ("Escape", "Escape", 27),
        "Backspace": ("Backspace", "Backspace", 8),
        "Tab": ("Tab", "Tab", 9),
        "ArrowDown": ("ArrowDown", "ArrowDown", 40),
        "ArrowUp": ("ArrowUp", "ArrowUp", 38),
        "ArrowLeft": ("ArrowLeft", "ArrowLeft", 37),
        "ArrowRight": ("ArrowRight", "ArrowRight", 39),
    }
    if key in common:
        key_name, code, vk = common[key]
        return {"key": key_name, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk}
    if len(key) == 1:
        vk = ord(key.upper())
        return {"key": key, "text": key, "code": f"Key{key.upper()}", "windowsVirtualKeyCode": vk}
    return {"key": key, "code": key}


def _remote_object_text(obj: Dict[str, Any]) -> str:
    if "value" in obj:
        value = obj.get("value")
        if isinstance(value, str):
            return value
        try:
            return json.dumps(value, ensure_ascii=False)
        except TypeError:
            return str(value)
    if "description" in obj:
        return str(obj.get("description") or "")
    if "unserializableValue" in obj:
        return str(obj.get("unserializableValue") or "")
    return str(obj.get("type") or "")


def browser_runtime_diagnostics(options: Optional[BrowserRuntimeOptions] = None) -> Dict[str, Any]:
    options = options or BrowserRuntimeOptions.from_env()
    active_ports = _active_devtools_ports(REAL_BROWSER_PROFILE_DIRS)
    return {
        "mode": options.normalized_mode(),
        "headless_env": _env_bool("LLM_BROWSER_HEADLESS", False),
        "cdp_http_url": options.cdp_http_url,
        "cdp_ws_url": _redact_url(options.cdp_ws_url),
        "chrome_path": str(options.chrome_path) if options.chrome_path else None,
        "profile_template": str(options.profile_template) if options.profile_template else None,
        "preserve_owned_profile": options.preserve_profile,
        "viewport": {"width": options.width, "height": options.height},
        "real_chrome": {
            "active_profile_ports": [{"profile_dir": str(base), "port": port} for base, port, _ in active_ports],
            "fallback_probe_ports": list(DEFAULT_CDP_PORTS),
        },
        "cloud": {
            "api_key_available": bool(options.cloud_api_key),
            "api_base": options.cloud_api_base,
            "profile_id": options.cloud_profile_id,
            "profile_name": options.cloud_profile_name,
            "proxy_country": options.cloud_proxy_country,
            "timeout": options.cloud_timeout,
            "allow_resizing": options.cloud_allow_resizing,
            "enable_recording": options.cloud_enable_recording,
            "custom_proxy_configured": bool(options.cloud_custom_proxy),
        },
        "daemon": {
            "name": options.daemon_name,
            "backend": options.daemon_backend,
        },
    }


def discover_real_browser_endpoint(
    profile_dirs: Sequence[Path] = REAL_BROWSER_PROFILE_DIRS,
    probe_ports: Sequence[int] = DEFAULT_CDP_PORTS,
    timeout_s: float = 15.0,
) -> DiscoveredCdpEndpoint:
    active_ports = _active_devtools_ports(profile_dirs)
    if active_ports:
        deadline = time.time() + timeout_s
        last_error: Optional[str] = None
        while True:
            for base, port, ws_path in active_ports:
                endpoint, error = _resolve_real_http_port(port, ws_path=ws_path, source=str(base))
                if endpoint is not None:
                    return endpoint
                if error:
                    last_error = error
            if time.time() >= deadline:
                break
            time.sleep(0.25)
        raise RuntimeError(
            "real Chrome DevToolsActivePort exists but CDP is not reachable yet. "
            "If Chrome opened a profile picker, choose the profile, then enable chrome://inspect/#remote-debugging "
            "and click Allow if Chrome asks. Last error: "
            f"{last_error or 'unknown'}"
        )

    for port in probe_ports:
        endpoint, _ = _resolve_real_http_port(port, ws_path="", source=f"127.0.0.1:{port}")
        if endpoint is not None:
            return endpoint

    raise RuntimeError(
        "real Chrome CDP endpoint not found. Open Chrome, visit chrome://inspect/#remote-debugging, "
        "enable remote debugging for this browser instance, then click Allow if Chrome prompts. "
        "For isolated automation, launch Chrome with --remote-debugging-port=9222 "
        "--user-data-dir=<non-default-dir> and pass --browser cdp --cdp-url http://127.0.0.1:9222."
    )


def _active_devtools_ports(profile_dirs: Sequence[Path]) -> List[Tuple[Path, str, str]]:
    active: List[Tuple[Path, str, str]] = []
    for base in profile_dirs:
        try:
            lines = (base / "DevToolsActivePort").read_text(encoding="utf-8").splitlines()
        except (FileNotFoundError, NotADirectoryError, OSError):
            continue
        port = lines[0].strip() if lines else ""
        ws_path = lines[1].strip() if len(lines) > 1 else ""
        if port:
            active.append((base, port, ws_path))
    return active


def _resolve_real_http_port(port: str, ws_path: str, source: str) -> Tuple[Optional[DiscoveredCdpEndpoint], Optional[str]]:
    http_url = f"http://127.0.0.1:{port}"
    try:
        response = requests.get(f"{http_url}/json/version", timeout=1)
    except requests.RequestException as exc:
        return None, str(exc)
    if response.status_code == 200:
        return DiscoveredCdpEndpoint(http_url=http_url, source=source), None
    if response.status_code == 404 and ws_path:
        return DiscoveredCdpEndpoint(websocket_url=f"ws://127.0.0.1:{port}{ws_path}", source=source), None
    return None, f"{http_url}/json/version returned HTTP {response.status_code}"


def _ws_from_devtools_active_port(http_url: str) -> Optional[str]:
    parsed = urlparse(http_url)
    if parsed.port is None:
        return None
    host = parsed.hostname or "127.0.0.1"
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    for _, port, ws_path in _active_devtools_ports(REAL_BROWSER_PROFILE_DIRS):
        if port == str(parsed.port) and ws_path:
            return f"ws://{host}:{port}{ws_path}"
    return None


def _get_json(url: str, timeout: float) -> Any:
    response = requests.get(url, timeout=timeout)
    response.raise_for_status()
    return response.json()


def _normalize_target_info(target: Dict[str, Any]) -> Dict[str, Any]:
    normalized = dict(target)
    if "id" not in normalized and "targetId" in normalized:
        normalized["id"] = normalized["targetId"]
    return normalized


def _cloud_browser_websocket_url(browser: Dict[str, Any]) -> str:
    for key in (
        "webSocketDebuggerUrl",
        "websocketUrl",
        "websocket_url",
        "wsUrl",
        "ws_url",
        "connectUrl",
        "connect_url",
        "cdpWsUrl",
        "cdp_ws_url",
    ):
        value = browser.get(key)
        if value:
            return str(value)
    cdp_url = browser.get("cdpUrl") or browser.get("cdp_url") or browser.get("devtoolsUrl") or browser.get("devtools_url")
    if not cdp_url:
        raise RuntimeError(f"Browser Use cloud browser response had no CDP endpoint fields: {sorted(browser.keys())}")
    version = _get_json(f"{str(cdp_url).rstrip('/')}/json/version", timeout=15)
    websocket_url = version.get("webSocketDebuggerUrl")
    if not websocket_url:
        raise RuntimeError(f"Browser Use cloud CDP endpoint returned no webSocketDebuggerUrl: {version}")
    return str(websocket_url)


def _browser_use_request(
    api_base: str,
    api_key: str,
    path: str,
    method: str,
    body: Optional[Dict[str, Any]] = None,
) -> Any:
    response = requests.request(
        method,
        f"{api_base.rstrip('/')}{path}",
        json=body,
        timeout=60,
        headers={"X-Browser-Use-API-Key": api_key, "Content-Type": "application/json"},
    )
    response.raise_for_status()
    if not response.content:
        return {}
    return response.json()


def _resolve_cloud_profile_name(api_base: str, api_key: str, profile_name: str) -> str:
    matches: List[Dict[str, Any]] = []
    page = 1
    while True:
        listing = _browser_use_request(
            api_base=api_base,
            api_key=api_key,
            path=f"/profiles?pageSize=100&pageNumber={page}",
            method="GET",
        )
        items = listing.get("items") if isinstance(listing, dict) else listing
        if not items:
            break
        for item in items:
            if isinstance(item, dict) and item.get("name") == profile_name:
                matches.append(item)
        if not isinstance(listing, dict):
            break
        total = listing.get("totalItems")
        if total is None or page * 100 >= int(total):
            break
        page += 1
    if not matches:
        raise RuntimeError(f"no Browser Use cloud profile named {profile_name!r}")
    if len(matches) > 1:
        raise RuntimeError(f"{len(matches)} Browser Use cloud profiles named {profile_name!r}; pass --cloud-profile-id")
    profile_id = matches[0].get("id")
    if not profile_id:
        raise RuntimeError(f"Browser Use cloud profile {profile_name!r} had no id")
    return str(profile_id)


def _stop_cloud_browser(api_base: str, api_key: str, browser_id: str) -> None:
    try:
        _browser_use_request(
            api_base=api_base,
            api_key=api_key,
            path=f"/browsers/{browser_id}",
            method="PATCH",
            body={"action": "stop"},
        )
    except BaseException:
        pass


def _first_env(*names: str) -> Optional[str]:
    for name in names:
        value = os.environ.get(name)
        if value:
            return value
    return None


def _env_path(name: str) -> Optional[Path]:
    value = os.environ.get(name)
    if not value:
        return None
    return Path(value).expanduser()


def _env_int(name: str, default: int) -> int:
    value = os.environ.get(name)
    if value is None or value == "":
        return default
    try:
        return int(value)
    except ValueError:
        return default


def _env_int_optional(name: str) -> Optional[int]:
    value = os.environ.get(name)
    if value is None or value == "":
        return None
    return int(value)


def _env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.lower() in {"1", "true", "yes", "on"}


def _env_bool_optional(name: str) -> Optional[bool]:
    value = os.environ.get(name)
    if value is None or value == "":
        return None
    return value.lower() in {"1", "true", "yes", "on"}


def _env_optional_value(name: str) -> Optional[str]:
    if name not in os.environ:
        return None
    return os.environ.get(name, "")


def _env_json_object(name: str) -> Optional[Dict[str, Any]]:
    value = os.environ.get(name)
    if not value:
        return None
    parsed = json.loads(value)
    if not isinstance(parsed, dict):
        raise RuntimeError(f"{name} must be a JSON object")
    return parsed


def _redact_url(url: Optional[str]) -> Optional[str]:
    if not url:
        return None
    parsed = urlparse(url)
    if not parsed.query and not parsed.params:
        return url
    safe = parsed._replace(query="...", params="")
    return safe.geturl()
