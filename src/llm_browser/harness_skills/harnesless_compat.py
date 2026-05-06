from __future__ import annotations

import json
from typing import Any, Dict

from llm_browser.harness.api import HelperAPI
from llm_browser.harness_skills.uploads import make_upload_file


SKILL = {
    "name": "harnesless_compat",
    "description": "Compatibility aliases and escape hatches from earlier browser-harness-style globals.",
    "exports": [
        "wait_until",
        "visible_text",
        "links",
        "dispatch_key",
        "load_helper",
        "save_helper",
    ],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    runtime = api.runtime

    def wait_until(expression: str, timeout_s: float = 20.0, timeout: float | None = None, interval_s: float = 0.25) -> Any:
        if timeout is not None:
            timeout_s = timeout
        api.check_cancel()
        return runtime.wait_until(expression, timeout_s=timeout_s, interval_s=interval_s)

    def dispatch_key(selector: str, key: str = "Enter", event: str = "keypress") -> Any:
        key_code = _keyboard_code(key)
        selector_json = json.dumps(selector)
        key_json = json.dumps(key)
        event_json = json.dumps(event)
        js = api.namespace["js"]
        return js(
            "(() => {"
            f"const e = document.querySelector({selector_json});"
            "if (!e) return false;"
            "e.focus();"
            f"e.dispatchEvent(new KeyboardEvent({event_json}, "
            f"{{key:{key_json}, code:{key_json}, keyCode:{key_code}, which:{key_code}, bubbles:true}}));"
            "return true;"
            "})()",
            await_promise=True,
        )

    def load_helper(path: str) -> None:
        from pathlib import Path

        helper_path = Path(path).expanduser()
        if not helper_path.is_absolute():
            helper_path = api.cwd / helper_path
        code = helper_path.read_text(encoding="utf-8")
        exec(compile(code, str(helper_path), "exec"), api.namespace, api.namespace)

    def save_helper(name: str, code: str) -> str:
        from pathlib import Path

        safe_name = "".join(ch if ch.isalnum() or ch in {"-", "_", "."} else "_" for ch in name)
        if not safe_name.endswith(".py"):
            safe_name += ".py"
        path = api.artifact_dir / "helpers" / safe_name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(code, encoding="utf-8")
        return str(path)

    return {
        "wait_until": wait_until,
        "visible_text": getattr(runtime, "visible_text", lambda max_chars=8000: ""),
        "links": getattr(runtime, "links", lambda limit=200: []),
        "dispatch_key": dispatch_key,
        "upload_file": make_upload_file(api),
        "load_helper": load_helper,
        "save_helper": save_helper,
    }


def _keyboard_code(key: str) -> int:
    codes = {
        "Enter": 13,
        "Tab": 9,
        "Escape": 27,
        "Backspace": 8,
        " ": 32,
        "ArrowLeft": 37,
        "ArrowUp": 38,
        "ArrowRight": 39,
        "ArrowDown": 40,
        "Delete": 46,
        "Home": 36,
        "End": 35,
        "PageUp": 33,
        "PageDown": 34,
    }
    if key in codes:
        return codes[key]
    return ord(key) if len(key) == 1 else 0
