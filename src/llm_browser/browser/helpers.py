from __future__ import annotations

import json
import math
import re
import sys
from pathlib import Path
from typing import Any, Dict, Optional, Tuple


INTERNAL_PAGE_PREFIXES = (
    "chrome://",
    "chrome-untrusted://",
    "devtools://",
    "chrome-extension://",
)


def is_internal_url(url: str) -> bool:
    if not url:
        return True
    if url == "about:blank":
        return True
    return url.startswith(INTERNAL_PAGE_PREFIXES)


def is_real_page_target(target: Dict[str, Any]) -> bool:
    target_type = target.get("type") or target.get("targetType")
    return target_type == "page" and not is_internal_url(str(target.get("url") or ""))


def wrap_js_if_return_statement(expression: str) -> str:
    if _is_bare_zero_arg_js_function(expression):
        return f"({expression})()"
    if _has_return_statement(expression) and not expression.lstrip().startswith("("):
        return f"(function(){{{expression}}})()"
    if _has_console_call(expression) and not expression.lstrip().startswith("("):
        return _wrap_console_capture(expression)
    return expression


def _is_bare_zero_arg_js_function(expression: str) -> bool:
    stripped = expression.strip()
    if not stripped:
        return False
    if re.search(r"\)\s*\(\s*\)\s*;?\s*$", stripped):
        return False
    return bool(
        re.match(r"^(async\s+)?\(\s*\)\s*=>", stripped)
        or re.match(r"^(\(\s*)?(async\s+)?function\s*\(\s*\)", stripped)
    )


def _has_console_call(expression: str) -> bool:
    return bool(re.search(r"\bconsole\.(?:log|warn|error|info|debug)\s*\(", expression))


def _wrap_console_capture(expression: str) -> str:
    async_prefix = "async " if re.search(r"\bawait\b", expression) else ""
    return (
        f"({async_prefix}function(){{"
        "const __butLogs=[];"
        "const __butConsole={log:console.log,warn:console.warn,error:console.error,info:console.info,debug:console.debug};"
        "const __butFormat=(value)=>{"
        "if(typeof value==='string')return value;"
        "if(value instanceof Error)return value.stack||value.message||String(value);"
        "try{return JSON.stringify(value);}catch(_){return String(value);}"
        "};"
        "for(const __butLevel of Object.keys(__butConsole)){"
        "console[__butLevel]=(...args)=>{"
        "__butLogs.push(args.map(__butFormat).join(' '));"
        "return __butConsole[__butLevel].apply(console,args);"
        "};"
        "}"
        "try{\n"
        f"{expression}\n"
        "}finally{Object.assign(console,__butConsole);}"
        "return __butLogs.join('\\n');"
        "})()"
    )


def decode_unserializable_js_value(value: str) -> Any:
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


KEYS: Dict[str, Tuple[int, str, str]] = {
    "Enter": (13, "Enter", "\r"),
    "Escape": (27, "Escape", ""),
    "Backspace": (8, "Backspace", ""),
    "Tab": (9, "Tab", "\t"),
    "Delete": (46, "Delete", ""),
    " ": (32, "Space", " "),
    "ArrowDown": (40, "ArrowDown", ""),
    "ArrowUp": (38, "ArrowUp", ""),
    "ArrowLeft": (37, "ArrowLeft", ""),
    "ArrowRight": (39, "ArrowRight", ""),
    "Home": (36, "Home", ""),
    "End": (35, "End", ""),
    "PageUp": (33, "PageUp", ""),
    "PageDown": (34, "PageDown", ""),
}


def key_event_for_cdp(key: str, modifiers: int = 0) -> Tuple[Dict[str, Any], str]:
    if key in KEYS:
        vk, code, text = KEYS[key]
    elif len(key) == 1:
        vk, code, text = ord(key.upper()), f"Key{key.upper()}", key
    else:
        vk, code, text = 0, key, ""
    base = {
        "key": key,
        "code": code,
        "modifiers": modifiers,
        "windowsVirtualKeyCode": vk,
        "nativeVirtualKeyCode": vk,
    }
    if text:
        base["text"] = text
    return base, text


def select_all_modifier() -> int:
    return 4 if sys.platform == "darwin" else 2


AGENT_HELPERS_TEMPLATE = '''"""Editable helpers for this browser use terminal session.

Keep task-specific browser routines here. This file is loaded into the
persistent Python browser namespace by reload_agent_helpers().
"""

from browser_helpers import *


def browser_state():
    return {
        "page": page_info(),
        "tabs": list_tabs(),
    }
'''


def ensure_agent_helpers_file(workspace: Path) -> Path:
    path = workspace / "agent_helpers.py"
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(AGENT_HELPERS_TEMPLATE, encoding="utf-8")
    return path


def _has_return_statement(expression: str) -> bool:
    index = 0
    length = len(expression)
    state = "code"
    quote = ""
    while index < length:
        char = expression[index]
        next_char = expression[index + 1] if index + 1 < length else ""
        if state == "code":
            if char in ("'", '"', "`"):
                state = "string"
                quote = char
                index += 1
                continue
            if char == "/" and next_char == "/":
                state = "line_comment"
                index += 2
                continue
            if char == "/" and next_char == "*":
                state = "block_comment"
                index += 2
                continue
            if expression.startswith("return", index):
                before = expression[index - 1] if index > 0 else ""
                after = expression[index + 6] if index + 6 < length else ""
                if not (before == "_" or before.isalnum()) and not (after == "_" or after.isalnum()):
                    return True
            index += 1
            continue
        if state == "line_comment":
            if char == "\n":
                state = "code"
            index += 1
            continue
        if state == "block_comment":
            if char == "*" and next_char == "/":
                state = "code"
                index += 2
                continue
            index += 1
            continue
        if state == "string":
            if char == "\\":
                index += 2
                continue
            if char == quote:
                state = "code"
                quote = ""
            index += 1
            continue
    return False
