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
import sys
import time
import urllib.request


INTERNAL = ("chrome://", "chrome-untrusted://", "devtools://", "chrome-extension://", "about:")


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
            if expression.startswith("return", i):
                before = expression[i - 1] if i > 0 else ""
                after = expression[i + 6] if i + 6 < n else ""
                if not (before == "_" or before.isalnum()) and not (after == "_" or after.isalnum()):
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


def goto_url(url):
    result = cdp("Page.navigate", url=url)
    wait_for_load(timeout=15)
    return result


def page_info():
    """Return url, title, viewport, scroll position, page size, and target info."""
    dialog = _send_meta("pending_dialog").get("dialog")
    if dialog:
        return {"dialog": dialog}
    expression = (
        "JSON.stringify({url:location.href,title:document.title,readyState:document.readyState,"
        "w:innerWidth,h:innerHeight,sx:scrollX,sy:scrollY,"
        "pw:document.documentElement.scrollWidth,ph:document.documentElement.scrollHeight})"
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
    time.sleep(seconds)


def _timeout_seconds(timeout):
    timeout = float(timeout)
    if timeout > 1000:
        timeout = timeout / 1000
    return min(timeout, 60.0)


def wait_for_load(timeout=15.0):
    timeout = _timeout_seconds(timeout)
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if js("document.readyState") == "complete":
                return True
        except Exception:
            pass
        time.sleep(0.3)
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
    deadline = time.time() + timeout
    while time.time() < deadline:
        if js(check):
            return True
        time.sleep(0.3)
    return False


def wait_for_network_idle(timeout=10.0, idle_ms=500):
    timeout = _timeout_seconds(timeout)
    deadline = time.time() + timeout
    last_activity = time.time()
    inflight = set()
    active_session = _send_meta("session").get("session_id")
    while time.time() < deadline:
        for event in drain_events():
            if event.get("session_id") != active_session:
                continue
            method = event.get("method", "")
            params = event.get("params", {})
            if method == "Network.requestWillBeSent":
                inflight.add(params.get("requestId"))
                last_activity = time.time()
            elif method in ("Network.loadingFinished", "Network.loadingFailed"):
                inflight.discard(params.get("requestId"))
                last_activity = time.time()
            elif method.startswith("Network."):
                last_activity = time.time()
        if not inflight and (time.time() - last_activity) * 1000 >= idle_ms:
            return True
        time.sleep(0.1)
    return False


def _write_b64_artifact(label, data_b64, suffix=".png", mime_type="image/png"):
    safe = "".join(ch if ch.isalnum() or ch in "-_" else "_" for ch in str(label or "screenshot")).strip("_") or "screenshot"
    path = ARTIFACT_DIR / f"{int(time.time() * 1000)}_{safe}{suffix}"
    path.write_bytes(base64.b64decode(data_b64))
    meta = {"path": str(path), "mime_type": mime_type, "detail": "auto", "label": label}
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
            time.sleep(0.2)
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


def press_key(key, modifiers=0):
    """Modifiers bitfield: 1=Alt, 2=Ctrl, 4=Meta(Cmd), 8=Shift."""
    vk, code, text = _KEYS.get(key) or _printable_key_metadata(key) or (0, key, "")
    base = {
        "key": key,
        "code": code,
        "modifiers": modifiers,
        "windowsVirtualKeyCode": vk,
        "nativeVirtualKeyCode": vk,
    }
    cdp("Input.dispatchKeyEvent", type="keyDown", **base, **({"text": text} if text else {}))
    cdp("Input.dispatchKeyEvent", type="keyUp", **base)
    return True


def scroll(x=0, y=0, dy=600, dx=0):
    cdp("Input.dispatchMouseEvent", type="mouseWheel", x=x, y=y, deltaX=dx, deltaY=dy)
    return True


def fill_input(selector, text, clear=True, clear_first=None, timeout=0.0):
    """Fill a framework-managed input using real key events plus input/change."""
    if clear_first is not None:
        clear = clear_first
    if timeout > 0 and not wait_for_element(selector, timeout=timeout):
        raise RuntimeError(f"fill_input: element not found: {selector!r}")
    focused = js(
        f"(()=>{{const e=document.querySelector({json.dumps(selector)});"
        "if(!e)return false;e.focus();return true;}})()"
    )
    if not focused:
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
    for ch in text:
        press_key(ch)
    js(
        f"(()=>{{const e=document.querySelector({json.dumps(selector)});"
        "if(!e)return;"
        "e.dispatchEvent(new Event('input',{bubbles:true}));"
        "e.dispatchEvent(new Event('change',{bubbles:true}));}})();"
    )
    return True


_KC = {"Enter": 13, "Tab": 9, "Escape": 27, "Backspace": 8, " ": 32, "ArrowLeft": 37, "ArrowUp": 38, "ArrowRight": 39, "ArrowDown": 40}


def dispatch_key(selector, key="Enter", event="keypress"):
    kc = _KC.get(key, ord(key) if len(key) == 1 else 0)
    js(
        f"(()=>{{const e=document.querySelector({json.dumps(selector)});"
        f"if(e){{e.focus();e.dispatchEvent(new KeyboardEvent({json.dumps(event)},"
        f"{{key:{json.dumps(key)},code:{json.dumps(key)},keyCode:{kc},which:{kc},bubbles:true}}));}}}})()"
    )


def upload_file(selector, path):
    doc = cdp("DOM.getDocument", depth=-1)
    node_id = cdp("DOM.querySelector", nodeId=doc["root"]["nodeId"], selector=selector)["nodeId"]
    if not node_id:
        raise RuntimeError(f"no element for {selector}")
    files = [path] if isinstance(path, str) else list(path)
    cdp("DOM.setFileInputFiles", files=files, nodeId=node_id)


def http_get(url, headers=None, timeout=20.0):
    request_headers = {"User-Agent": "Mozilla/5.0", "Accept-Encoding": "gzip"}
    if headers:
        request_headers.update(headers)
    with urllib.request.urlopen(urllib.request.Request(url, headers=request_headers), timeout=timeout) as response:
        data = response.read()
        if response.headers.get("Content-Encoding") == "gzip":
            data = gzip.decompress(data)
        content_type = response.headers.get("Content-Type", "")
        if "text" in content_type or "json" in content_type or "html" in content_type:
            return data.decode()
        return data
