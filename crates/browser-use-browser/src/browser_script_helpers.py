"""Browser-script helpers.

Rust owns the CDP websocket and session state. This file owns the
LLM-readable browser interaction helpers. Keep these helpers close to
browser-harness semantics so the model sees one coherent browser API.
"""

import base64
import gzip
import json
import math
import os
import pathlib
import sys
import time as _time
import urllib.error
import urllib.request
from urllib.parse import urlparse


INTERNAL = ("chrome://", "chrome-untrusted://", "devtools://", "chrome-extension://", "about:")
__last_domain_skills = []


def _send_meta(meta, **params):
    return _bridge({"kind": "meta", "meta": meta, **params})


def cdp(method, session_id=None, **params):
    """Raw CDP. Example: cdp("Page.navigate", url="https://example.com")."""
    return _bridge({"kind": "cdp", "method": method, "session_id": session_id, "params": params})


def cdp_batch(calls):
    out = []
    for call in calls:
        if isinstance(call, dict):
            call = dict(call)
            method = call.pop("method")
            session_id = call.pop("session_id", None)
            out.append(cdp(method, session_id=session_id, **call))
        else:
            method, params = call
            out.append(cdp(method, **params))
    return out


def drain_events():
    return _send_meta("drain_events").get("events", [])


def _js_snippet(expression, limit=160):
    snippet = expression.strip().replace("\n", "\\n")
    return snippet[: limit - 3] + "..." if len(snippet) > limit else snippet


def _js_exception_description(result, details):
    desc = result.get("description")
    exc = details.get("exception") if details else None
    if not desc and isinstance(exc, dict):
        desc = exc.get("description")
        if desc is None and "value" in exc:
            desc = str(exc["value"])
        if desc is None:
            desc = exc.get("className")
    if not desc and details:
        desc = details.get("text")
    return desc or "JavaScript evaluation failed"


def _decode_unserializable_js_value(value):
    if value == "NaN":
        return math.nan
    if value == "Infinity":
        return math.inf
    if value == "-Infinity":
        return -math.inf
    if value == "-0":
        return -0.0
    if value.endswith("n"):
        return int(value[:-1])
    return value


def _runtime_value(response, expression):
    result = response.get("result", {})
    details = response.get("exceptionDetails")
    if details or result.get("subtype") == "error":
        desc = _js_exception_description(result, details)
        if details:
            line = details.get("lineNumber")
            col = details.get("columnNumber")
            loc = f" at line {line}, column {col}" if line is not None and col is not None else ""
        else:
            loc = ""
        raise RuntimeError(f"JavaScript evaluation failed{loc}: {desc}; expression: {_js_snippet(expression)}")
    if "value" in result:
        return result["value"]
    if "unserializableValue" in result:
        return _decode_unserializable_js_value(result["unserializableValue"])
    return None


def _runtime_evaluate(expression, session_id=None, await_promise=False, return_by_value=True):
    try:
        response = cdp(
            "Runtime.evaluate",
            session_id=session_id,
            expression=expression,
            returnByValue=return_by_value,
            awaitPromise=await_promise,
        )
    except TimeoutError as exc:
        raise RuntimeError(f"Runtime.evaluate timed out; expression: {_js_snippet(expression)}") from exc
    return _runtime_value(response, expression)


def _has_return_statement(expression):
    i = 0
    n = len(expression)
    state = "code"
    quote = ""
    brace_stack = []
    pending_function_body = False
    pending_arrow_body = False

    def is_ident(ch):
        return ch == "_" or ch == "$" or ch.isalnum()

    def is_keyword_at(keyword, pos):
        if not expression.startswith(keyword, pos):
            return False
        before = expression[pos - 1] if pos > 0 else ""
        after_pos = pos + len(keyword)
        after = expression[after_pos] if after_pos < n else ""
        return not is_ident(before) and not is_ident(after)

    def next_nonspace(pos):
        while pos < n and expression[pos].isspace():
            pos += 1
        return expression[pos] if pos < n else ""

    while i < n:
        ch = expression[i]
        nxt = expression[i + 1] if i + 1 < n else ""
        if state == "code":
            if ch in ("'", '"', "`"):
                state = "string"
                quote = ch
                i += 1
                continue
            if ch == "/" and nxt == "/":
                state = "line_comment"
                i += 2
                continue
            if ch == "/" and nxt == "*":
                state = "block_comment"
                i += 2
                continue
            if ch == "=" and nxt == ">":
                pending_arrow_body = True
                i += 2
                continue
            if pending_arrow_body and not ch.isspace() and ch != "{":
                pending_arrow_body = False
            if ch == "{":
                brace_stack.append(pending_function_body or pending_arrow_body)
                pending_function_body = False
                pending_arrow_body = False
                i += 1
                continue
            if ch == "}":
                if brace_stack:
                    brace_stack.pop()
                i += 1
                continue
            if is_keyword_at("function", i):
                pending_function_body = True
                i += len("function")
                continue
            if is_keyword_at("return", i):
                inside_nested_function = any(brace_stack)
                looks_like_property_key = next_nonspace(i + len("return")) == ":"
                if not inside_nested_function and not looks_like_property_key:
                    return True
            i += 1
            continue
        if state == "line_comment":
            if ch == "\n":
                state = "code"
            i += 1
            continue
        if state == "block_comment":
            if ch == "*" and nxt == "/":
                state = "code"
                i += 2
                continue
            i += 1
            continue
        if state == "string":
            if ch == "\\":
                i += 2
                continue
            if ch == quote:
                state = "code"
                quote = ""
            i += 1
            continue
    return False


def js(expression, target_id=None, returnByValue=True):
    """Run JS in the attached tab, or in an iframe target via target_id.

    Expressions with top-level `return` are wrapped in an IIFE, so both
    `document.title` and `const x = 1; return x` are valid.
    """
    session_id = cdp("Target.attachToTarget", targetId=target_id, flatten=True)["sessionId"] if target_id else None
    if _has_return_statement(expression) and not expression.strip().startswith("("):
        expression = f"(function(){{{expression}}})()"
    return _runtime_evaluate(
        expression,
        session_id=session_id,
        await_promise=True,
        return_by_value=returnByValue,
    )


def _truthy_env(name, default=False):
    value = os.environ.get(name)
    if value is None:
        return default
    return value.strip().lower() not in ("", "0", "false", "no", "off")


def _domain_skill_roots():
    roots = []
    configured = os.environ.get("BH_DOMAIN_SKILLS_ROOT") or os.environ.get("BH_DOMAIN_SKILLS_DIR")
    if configured:
        roots.extend(pathlib.Path(part).expanduser() for part in configured.split(os.pathsep) if part.strip())
    for root in globals().get("DOMAIN_SKILL_ROOTS", []):
        roots.append(pathlib.Path(root).expanduser())
    try:
        roots.append(pathlib.Path(agent_workspace()) / "domain-skills")
    except Exception:
        pass

    seen = set()
    out = []
    for root in roots:
        try:
            resolved = root.resolve()
        except Exception:
            resolved = root
        key = str(resolved)
        if key in seen:
            continue
        seen.add(key)
        if resolved.is_dir():
            out.append(resolved)
    return out


def _domain_from_url(value):
    value = str(value or "").strip()
    parsed = urlparse(value if "://" in value else f"https://{value}")
    host = (parsed.hostname or value.split("/", 1)[0]).strip().lower()
    if host.startswith("www."):
        host = host[4:]
    return host


def _domain_skill_aliases(url_or_domain):
    host = _domain_from_url(url_or_domain)
    aliases = {host, host.replace(".", "-")}
    labels = [part for part in host.split(".") if part]
    if labels:
        aliases.add(labels[0])
    if len(labels) >= 2:
        aliases.add(labels[-2])
        aliases.add(f"{labels[-2]}-{labels[-1]}")
    if len(labels) >= 3:
        aliases.add(f"{labels[-2]}-{labels[0]}")
        aliases.add(f"{labels[0]}-{labels[-2]}")
    return {alias.lower().replace("_", "-") for alias in aliases if alias}


def _domain_skills_enabled():
    if os.environ.get("BH_DOMAIN_SKILLS") is not None:
        return _truthy_env("BH_DOMAIN_SKILLS")
    return bool(_domain_skill_roots())


def domain_skills_for_url(url_or_domain, include_content=False, max_files=10, max_bytes=120000):
    """Return matching browser-harness domain-skill files for a URL/domain.

    Set include_content=True when the task is site-specific and the model needs
    the playbook before inventing selectors, private API routes, or flows.
    """
    aliases = _domain_skill_aliases(url_or_domain)
    matches = []
    remaining = int(max_bytes)
    for root in _domain_skill_roots():
        try:
            entries = sorted(root.iterdir(), key=lambda path: path.name.lower())
        except OSError:
            continue
        for site_dir in entries:
            if not site_dir.is_dir():
                continue
            site_key = site_dir.name.lower().replace("_", "-")
            if site_key not in aliases:
                continue
            files = []
            for path in sorted(site_dir.rglob("*")):
                if not path.is_file() or path.suffix.lower() not in (".md", ".py"):
                    continue
                rel = path.relative_to(site_dir).as_posix()
                item = {"name": rel, "path": str(path)}
                if include_content and remaining > 0:
                    try:
                        content = path.read_text(encoding="utf-8", errors="replace")
                    except OSError as exc:
                        content = f"[failed to read domain skill: {exc}]"
                    encoded = content[:remaining]
                    item["content"] = encoded
                    item["truncated"] = len(encoded) < len(content)
                    remaining -= len(encoded)
                files.append(item)
                if len(files) >= max_files:
                    break
            if files:
                matches.append({"site": site_dir.name, "root": str(root), "files": files})
    return matches


def last_domain_skills(include_content=False):
    if not __last_domain_skills:
        return []
    if include_content:
        url = __last_domain_skills[0].get("url") if isinstance(__last_domain_skills[0], dict) else None
        if url:
            return domain_skills_for_url(url, include_content=True)
    return __last_domain_skills


def goto_url(url):
    global __last_domain_skills
    result = cdp("Page.navigate", url=url)
    __last_domain_skills = []
    if _domain_skills_enabled():
        skills = domain_skills_for_url(url, include_content=False)
        if skills:
            __last_domain_skills = [{"url": url, **skill} for skill in skills]
            result = {**result, "domain_skills": __last_domain_skills}
    wait_for_load(timeout=15)
    return result


def page_info():
    """Return url, title, viewport, scroll position, page size, and target info."""
    dialog = _send_meta("pending_dialog").get("dialog")
    if dialog:
        return {"dialog": dialog}
    expression = (
        "(()=>{"
        "const root=document.documentElement||document.body||{};"
        "return JSON.stringify({url:location.href,title:document.title||'',readyState:document.readyState||'',"
        "w:innerWidth,h:innerHeight,sx:scrollX||0,sy:scrollY||0,"
        "pw:root.scrollWidth||innerWidth,ph:root.scrollHeight||innerHeight});"
        "})()"
    )
    info = json.loads(_runtime_evaluate(expression))
    info["target"] = current_tab()
    return info


def current_tab():
    page = _send_meta("current_tab")
    target_id = page.get("targetId") or page.get("target_id")
    session_id = page.get("sessionId") or page.get("session_id")
    return {
        "targetId": target_id,
        "target_id": target_id,
        "sessionId": session_id,
        "session_id": session_id,
        "url": page.get("url", ""),
        "title": page.get("title", ""),
    }


def list_tabs(include_chrome=True):
    out = []
    for target in cdp("Target.getTargets").get("targetInfos", []):
        if target.get("type") != "page":
            continue
        url = target.get("url", "")
        if not include_chrome and url.startswith(INTERNAL):
            continue
        target_id = target.get("targetId")
        out.append(
            {
                "targetId": target_id,
                "target_id": target_id,
                "title": target.get("title", ""),
                "url": url,
            }
        )
    return out


def _mark_tab():
    # Kept as a no-op compatibility hook. Browser-harness marks tab titles for
    # visibility, but here Rust tracks the current target explicitly.
    return None


def switch_tab(target):
    """Switch to a tab by raw target id or a tab dict returned by current_tab/list_tabs."""
    target_id = target.get("targetId") or target.get("target_id") if isinstance(target, dict) else target
    if not target_id:
        raise RuntimeError("switch_tab requires target_id")
    cdp("Target.activateTarget", targetId=target_id)
    session_id = cdp("Target.attachToTarget", targetId=target_id, flatten=True)["sessionId"]
    _send_meta("set_session", target_id=target_id, session_id=session_id)
    _mark_tab()
    return session_id


def new_tab(url="about:blank"):
    # Match browser-harness: create blank first, attach, then navigate. Passing
    # the final URL to createTarget can race with attach/load polling.
    target_id = cdp("Target.createTarget", url="about:blank")["targetId"]
    switch_tab(target_id)
    if url != "about:blank":
        goto_url(url)
    return target_id


def ensure_real_tab():
    tabs = list_tabs(include_chrome=False)
    if not tabs:
        return None
    try:
        current = current_tab()
        if current["url"] and not current["url"].startswith(INTERNAL):
            return current
    except Exception:
        pass
    switch_tab(tabs[0])
    return tabs[0]


def iframe_target(url_substr):
    for target in cdp("Target.getTargets").get("targetInfos", []):
        if target.get("type") == "iframe" and url_substr in target.get("url", ""):
            return target.get("targetId")
    return None


def wait(seconds=1.0):
    _time.sleep(seconds)


def _timeout_seconds(timeout):
    timeout = float(timeout)
    if timeout > 1000:
        timeout = timeout / 1000
    return min(timeout, 60.0)


def wait_for_load(timeout=15.0):
    timeout = _timeout_seconds(timeout)
    deadline = _time.time() + timeout
    while _time.time() < deadline:
        try:
            if js("document.readyState") == "complete":
                return True
        except Exception:
            pass
        _time.sleep(0.3)
    return False


def wait_for_element(selector, timeout=10.0, visible=False):
    timeout = _timeout_seconds(timeout)
    if visible:
        check = (
            f"(()=>{{const e=document.querySelector({json.dumps(selector)});"
            "if(!e)return false;"
            "if(typeof e.checkVisibility==='function')"
            "return e.checkVisibility({checkOpacity:true,checkVisibilityCSS:true});"
            "const s=getComputedStyle(e);"
            "return s.display!=='none'&&s.visibility!=='hidden'&&s.opacity!=='0'}})()"
        )
    else:
        check = f"!!document.querySelector({json.dumps(selector)})"
    deadline = _time.time() + timeout
    while _time.time() < deadline:
        if js(check):
            return True
        _time.sleep(0.3)
    return False


def wait_for_network_idle(timeout=10.0, idle_ms=500):
    timeout = _timeout_seconds(timeout)
    deadline = _time.time() + timeout
    last_activity = _time.time()
    inflight = set()
    active_session = _send_meta("session").get("session_id")
    while _time.time() < deadline:
        for event in drain_events():
            if event.get("session_id") != active_session:
                continue
            method = event.get("method", "")
            params = event.get("params", {})
            if method == "Network.requestWillBeSent":
                inflight.add(params.get("requestId"))
                last_activity = _time.time()
            elif method in ("Network.loadingFinished", "Network.loadingFailed"):
                inflight.discard(params.get("requestId"))
                last_activity = _time.time()
            elif method.startswith("Network."):
                last_activity = _time.time()
        if not inflight and (_time.time() - last_activity) * 1000 >= idle_ms:
            return True
        _time.sleep(0.1)
    return False


def _write_b64_artifact(label, data_b64, suffix=".png", mime_type="image/png"):
    safe = "".join(ch if ch.isalnum() or ch in "-_" else "_" for ch in str(label or "screenshot")).strip("_") or "screenshot"
    path = ARTIFACT_DIR / f"{int(_time.time() * 1000)}_{safe}{suffix}"
    path.write_bytes(base64.b64decode(data_b64))
    meta = {"path": str(path), "mime_type": mime_type, "detail": "auto", "label": label, "source": "screenshot"}
    __images.append(meta)
    __artifacts.append({"path": str(path), "kind": "image", "mime_type": mime_type})
    return str(path)


def capture_screenshot(label="screenshot", full=False, attach=True, max_dim=None, **kwargs):
    """Save a PNG of the current viewport and return its local artifact path."""
    try:
        target_id = (current_tab() or {}).get("targetId")
        if target_id:
            cdp("Target.activateTarget", session_id=None, targetId=target_id)
        cdp("Page.bringToFront")
        version = cdp("Browser.getVersion", session_id=None)
        if "Headless" in (version.get("userAgent") or ""):
            cdp("Emulation.setDeviceMetricsOverride", width=1280, height=720, deviceScaleFactor=1, mobile=False)
            _time.sleep(0.2)
    except Exception:
        pass
    params = {"format": kwargs.pop("format", "png")}
    if full:
        params["captureBeyondViewport"] = True
    params.update(kwargs)
    result = cdp("Page.captureScreenshot", **params)
    if not attach:
        return result
    path = _write_b64_artifact(label, result["data"], ".png", "image/png")
    if max_dim:
        try:
            from PIL import Image

            img = Image.open(path)
            if max(img.size) > max_dim:
                img.thumbnail((max_dim, max_dim))
                img.save(path)
        except Exception:
            pass
    return path


def screenshot(label="screenshot", full=False):
    return capture_screenshot(label=label, full=full, attach=True)


def screenshot_clip(label, x, y, width, height):
    return capture_screenshot(label=label, clip={"x": x, "y": y, "width": width, "height": height, "scale": 1}, attach=True)


def click_at_xy(x, y, button="left", clicks=1):
    cdp("Input.dispatchMouseEvent", type="mousePressed", x=x, y=y, button=button, clickCount=clicks)
    cdp("Input.dispatchMouseEvent", type="mouseReleased", x=x, y=y, button=button, clickCount=clicks)
    return True


def type_text(text):
    cdp("Input.insertText", text=text)
    return True


_KEYS = {
    "Enter": (13, "Enter", "\r"),
    "Tab": (9, "Tab", "\t"),
    "Backspace": (8, "Backspace", ""),
    "Escape": (27, "Escape", ""),
    "Delete": (46, "Delete", ""),
    " ": (32, "Space", " "),
    "ArrowLeft": (37, "ArrowLeft", ""),
    "ArrowUp": (38, "ArrowUp", ""),
    "ArrowRight": (39, "ArrowRight", ""),
    "ArrowDown": (40, "ArrowDown", ""),
    "Home": (36, "Home", ""),
    "End": (35, "End", ""),
    "PageUp": (33, "PageUp", ""),
    "PageDown": (34, "PageDown", ""),
}

_PRINTABLE_KEY_CODES = {
    "-": (189, "Minus"),
    "=": (187, "Equal"),
    "[": (219, "BracketLeft"),
    "]": (221, "BracketRight"),
    "\\": (220, "Backslash"),
    ";": (186, "Semicolon"),
    "'": (222, "Quote"),
    ",": (188, "Comma"),
    ".": (190, "Period"),
    "/": (191, "Slash"),
    "`": (192, "Backquote"),
}

_MODIFIER_BITS = {
    "alt": 1,
    "option": 1,
    "ctrl": 2,
    "control": 2,
    "cmd": 4,
    "command": 4,
    "meta": 4,
    "shift": 8,
}


def _printable_key_metadata(key):
    if len(key) != 1:
        return None
    if key.isalpha():
        upper = key.upper()
        return ord(upper), f"Key{upper}", key
    if key.isdigit():
        return ord(key), f"Digit{key}", key
    if key in _PRINTABLE_KEY_CODES:
        vk, code = _PRINTABLE_KEY_CODES[key]
        return vk, code, key
    return ord(key), key, key


def _parse_key_chord(key, modifiers):
    if not isinstance(key, str) or "+" not in key:
        return key, modifiers
    parts = [part.strip() for part in key.split("+") if part.strip()]
    if len(parts) < 2:
        return key, modifiers
    parsed_modifiers = modifiers
    for part in parts[:-1]:
        bit = _MODIFIER_BITS.get(part.lower())
        if bit is None:
            return key, modifiers
        parsed_modifiers |= bit
    parsed_key = parts[-1]
    if parsed_key.lower() == "space":
        parsed_key = " "
    return parsed_key, parsed_modifiers


def press_key(key, modifiers=0):
    """Modifiers bitfield: 1=Alt, 2=Ctrl, 4=Meta(Cmd), 8=Shift. Chords like "Meta+A" also work."""
    key, modifiers = _parse_key_chord(key, modifiers)
    vk, code, text = _KEYS.get(key) or _printable_key_metadata(key) or (0, key, "")
    base = {
        "key": key,
        "code": code,
        "modifiers": modifiers,
        "windowsVirtualKeyCode": vk,
        "nativeVirtualKeyCode": vk,
    }
    event_type = "rawKeyDown" if modifiers else "keyDown"
    cdp("Input.dispatchKeyEvent", type=event_type, **base, **({"text": text} if text and not modifiers else {}))
    cdp("Input.dispatchKeyEvent", type="keyUp", **base)
    return True


def scroll(x=0, y=0, dy=600, dx=0):
    cdp("Input.dispatchMouseEvent", type="mouseWheel", x=x, y=y, deltaX=dx, deltaY=dy)
    return True


def _query_selector_node_id(selector):
    doc = cdp("DOM.getDocument", depth=0)
    root = (doc or {}).get("root") or {}
    root_id = root.get("nodeId")
    if not root_id:
        return None
    result = cdp("DOM.querySelector", nodeId=root_id, selector=selector)
    node_id = (result or {}).get("nodeId")
    return node_id or None


def _wait_for_selector_node_id(selector, timeout=0.0):
    deadline = _time.monotonic() + _timeout_seconds(timeout)
    while True:
        node_id = _query_selector_node_id(selector)
        if node_id:
            return node_id
        if timeout <= 0 or _time.monotonic() >= deadline:
            return None
        _time.sleep(0.1)


def _quad_center(quad):
    if not quad or len(quad) < 8:
        return None
    xs = quad[0::2]
    ys = quad[1::2]
    if max(xs) <= min(xs) or max(ys) <= min(ys):
        return None
    return (min(xs) + max(xs)) / 2, (min(ys) + max(ys)) / 2


def _node_center(node_id):
    try:
        model = (cdp("DOM.getBoxModel", nodeId=node_id) or {}).get("model") or {}
    except Exception:
        return None
    return _quad_center(model.get("border")) or _quad_center(model.get("content"))


def _focus_selector_like_user(selector, timeout=0.0):
    node_id = _wait_for_selector_node_id(selector, timeout=timeout)
    if not node_id:
        return False
    try:
        cdp("DOM.scrollIntoViewIfNeeded", nodeId=node_id)
    except Exception:
        pass
    center = _node_center(node_id)
    if center:
        click_at_xy(center[0], center[1])
        return True
    try:
        cdp("DOM.focus", nodeId=node_id)
        return True
    except Exception:
        return False


def fill_input(selector, text, clear=True, clear_first=None, timeout=0.0):
    """Fill an input by focusing it through CDP, then using browser input events."""
    if clear_first is not None:
        clear = clear_first
    if not _focus_selector_like_user(selector, timeout=timeout):
        raise RuntimeError(f"fill_input: element not found: {selector!r}")
    if clear:
        mods = 4 if sys.platform == "darwin" else 2
        select_all = {
            "key": "a",
            "code": "KeyA",
            "modifiers": mods,
            "windowsVirtualKeyCode": 65,
            "nativeVirtualKeyCode": 65,
        }
        cdp("Input.dispatchKeyEvent", type="rawKeyDown", **select_all)
        cdp("Input.dispatchKeyEvent", type="keyUp", **select_all)
        press_key("Backspace")
    if text:
        type_text(str(text))
    return True


def upload_file(selector, path):
    doc = cdp("DOM.getDocument", depth=-1)
    node_id = cdp("DOM.querySelector", nodeId=doc["root"]["nodeId"], selector=selector)["nodeId"]
    if not node_id:
        raise RuntimeError(f"no element for {selector}")
    files = [path] if isinstance(path, str) else list(path)
    cdp("DOM.setFileInputFiles", files=files, nodeId=node_id)


class _HttpGetText(str):
    def __new__(cls, value, status_code=None, headers=None, url=None):
        obj = str.__new__(cls, value)
        obj.status_code = status_code
        obj.status = status_code
        obj.headers = headers or {}
        obj.url = url
        return obj

    @property
    def text(self):
        return str(self)

    @property
    def content(self):
        return str(self).encode("utf-8")

    def json(self):
        return json.loads(str(self))


class _HttpGetBytes(bytes):
    def __new__(cls, value, status_code=None, headers=None, url=None):
        obj = bytes.__new__(cls, value)
        obj.status_code = status_code
        obj.status = status_code
        obj.headers = headers or {}
        obj.url = url
        return obj

    @property
    def content(self):
        return bytes(self)

    @property
    def text(self):
        return bytes(self).decode("utf-8", errors="replace")

    def json(self):
        return json.loads(self.text)


def http_get(url, headers=None, timeout=20.0, binary=None):
    """Pure HTTP fetch for static pages and APIs.

    When BROWSER_USE_API_KEY is set and fetch_use is installed, route through
    fetch-use like browser-harness. Otherwise fall back to local urllib with a
    browser-like UA and gzip handling. Pass binary=True for bytes.
    """
    if os.environ.get("BROWSER_USE_API_KEY"):
        try:
            from fetch_use import fetch_sync

            response = fetch_sync(url, headers=headers, timeout_ms=int(float(timeout) * 1000))
            status_code = getattr(response, "status_code", getattr(response, "status", None))
            response_headers = dict(getattr(response, "headers", {}) or {})
            response_url = getattr(response, "url", url)
            if binary is True:
                data = getattr(response, "content", None)
                if data is None:
                    data = getattr(response, "body", None)
                if data is None:
                    data = getattr(response, "text", "").encode("utf-8", errors="replace")
                elif isinstance(data, str):
                    data = data.encode("utf-8", errors="replace")
                else:
                    data = bytes(data)
                return _HttpGetBytes(data, status_code, response_headers, response_url)
            return _HttpGetText(
                response.text,
                status_code,
                response_headers,
                response_url,
            )
        except ImportError:
            pass
    request_headers = {"User-Agent": "Mozilla/5.0", "Accept-Encoding": "gzip"}
    if headers:
        request_headers.update(headers)
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=request_headers), timeout=timeout) as response:
            data = response.read()
            if response.headers.get("Content-Encoding") == "gzip":
                data = gzip.decompress(data)
            content_type = response.headers.get("Content-Type", "")
            response_headers = dict(response.headers.items())
            status_code = getattr(response, "status", None) or response.getcode()
            if binary is True:
                return _HttpGetBytes(data, status_code, response_headers, response.geturl())
            if binary is False or "text" in content_type or "json" in content_type or "html" in content_type:
                charset = response.headers.get_content_charset() or "utf-8"
                return _HttpGetText(data.decode(charset, errors="replace"), status_code, response_headers, response.geturl())
            return _HttpGetBytes(data, status_code, response_headers, response.geturl())
    except urllib.error.HTTPError as exc:
        guidance = (
            "http_get received HTTP "
            f"{exc.code} for {url}. If this is bot/login protection, retry from the browser with js(fetch(...)), "
            "pass site-specific headers/cookies, or configure the Browser Use fetch proxy with BROWSER_USE_API_KEY."
        )
        raise RuntimeError(guidance) from exc
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise RuntimeError(
            f"http_get failed for {url}: {exc}. Try a shorter timeout, browser js(fetch(...)), or a configured proxy if the site blocks direct HTTP."
        ) from exc
