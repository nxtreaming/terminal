from __future__ import annotations

import json
import re
from copy import deepcopy
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple


MAX_SUMMARY_CHARS = 18000
MAX_KEPT_TEXT_CHARS = 7000


def message_chars(messages: List[Dict[str, Any]]) -> int:
    return sum(len(_message_text(message)) for message in messages)


def compact_messages(
    messages: List[Dict[str, Any]],
    artifact_dir: Path,
    keep_last: int = 12,
    session_events: Optional[List[Dict[str, Any]]] = None,
) -> Tuple[List[Dict[str, Any]], Path]:
    if len(messages) <= keep_last + 1:
        return messages, artifact_dir / "compactions" / "noop.json"

    keep_start = _valid_suffix_start(messages, max(0, len(messages) - keep_last))
    kept = [_trim_message(message) for message in messages[keep_start:]]
    summary = _summary(messages[:keep_start], session_events=session_events or [])
    compaction_dir = artifact_dir / "compactions"
    compaction_dir.mkdir(parents=True, exist_ok=True)
    path = compaction_dir / f"{len(list(compaction_dir.glob('*.json'))) + 1:03d}.json"
    payload = {"summary": summary, "kept_messages": len(kept), "original_messages": len(messages)}
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    compacted = [
        {
            "role": "user",
            "content": (
                "Conversation was compacted by browser use terminal. "
                "Use this summary plus the recent messages and artifact paths to continue.\n\n"
                f"{summary}\n\nFull compaction artifact: {path}"
            ),
        }
    ]
    compacted.extend(kept)
    return compacted, path


def _valid_suffix_start(messages: List[Dict[str, Any]], desired_start: int) -> int:
    """Move the compaction boundary to a provider-valid message boundary.

    Responses-style providers reject a function_call_output unless the same
    request also contains the matching function_call, or the request is chained
    to the response that produced that call. After local compaction we replay a
    compacted transcript, so a suffix that starts with a tool message is invalid.
    Move the boundary back to the assistant turn that created the leading tool
    output and keep that whole assistant/tool block intact.
    """

    if desired_start <= 0:
        return 0
    if desired_start >= len(messages):
        return len(messages)
    start = desired_start
    while start > 0 and messages[start].get("role") == "tool":
        previous = start - 1
        while previous >= 0 and messages[previous].get("role") == "tool":
            previous -= 1
        if previous >= 0 and messages[previous].get("role") == "assistant":
            return previous
        start += 1
        if start >= len(messages):
            return len(messages)
    return start


def _summary(messages: List[Dict[str, Any]], session_events: List[Dict[str, Any]]) -> str:
    first_user = ""
    recent_tools = []
    tool_refs = []
    errors = []
    paths = []
    for message in messages:
        role = message.get("role")
        text = _message_text(message)
        if role == "user" and not first_user:
            first_user = text[:3000]
        if role == "tool":
            if text:
                recent_tools.append(_compact_text(text, 2600))
            if "output_path" in text or "artifact" in text or "screenshots" in text:
                tool_refs.append(_compact_text(text, 1600))
            if "tool error" in text or "'ok': False" in text or '"ok": false' in text:
                errors.append(_compact_text(text, 1600))
        for path in _extract_paths(text):
            if path not in paths:
                paths.append(path)
    parts = []
    if first_user:
        parts.append(f"Original user/task goal:\n{first_user}")
    if recent_tools:
        parts.append("Recent tool results before compaction:\n" + "\n\n".join(recent_tools[-10:]))
    if tool_refs:
        parts.append("Important tool/artifact references:\n" + "\n\n".join(tool_refs[-8:]))
    if paths:
        parts.append("Known artifact/file paths:\n" + "\n".join(paths[-40:]))
    if errors:
        parts.append("Recent recoverable errors:\n" + "\n\n".join(errors[-5:]))
    event_summary = _event_summary(session_events)
    if event_summary:
        parts.append(event_summary)
    if not parts:
        parts.append(f"Compacted {len(messages)} older message(s). Continue from recent context.")
    return _compact_text("\n\n".join(parts), MAX_SUMMARY_CHARS)


def _event_summary(events: List[Dict[str, Any]]) -> str:
    if not events:
        return ""
    image_lines: List[str] = []
    rehydrate_lines: List[str] = []
    browser_lines: List[str] = []
    status_lines: List[str] = []
    for event in events:
        event_type = str(event.get("type") or "")
        payload = event.get("payload") if isinstance(event.get("payload"), dict) else {}
        if event_type == "tool.image":
            image = payload.get("image") if isinstance(payload.get("image"), dict) else {}
            label = str(image.get("label") or "screenshot")
            path = str(image.get("path") or "")
            url = str(image.get("url") or "")
            title = str(image.get("title") or "")
            bits = [label]
            if title:
                bits.append(f"title={title[:120]}")
            if url:
                bits.append(f"url={url[:180]}")
            if path:
                bits.append(f"path={path}")
                rehydrate_lines.append(f"attach_image({path!r}, label={label!r})")
            image_lines.append(" | ".join(bits))
        elif event_type in {"tool.failed", "session.failed", "session.cancelled"}:
            text = str(payload.get("error") or payload.get("reason") or "")
            status_lines.append(f"{event_type}: {text[:300]}")
        elif event_type == "tool.finished":
            output = payload.get("output") if isinstance(payload.get("output"), dict) else {}
            data = output.get("data") if isinstance(output.get("data"), dict) else {}
            trace_path = data.get("path") or data.get("output_path")
            if trace_path:
                browser_lines.append(f"{payload.get('name', 'tool')} artifact: {trace_path}")
    parts = []
    if image_lines:
        parts.append("Recent screenshot timeline:\n" + "\n".join(image_lines[-16:]))
    if rehydrate_lines:
        parts.append("Screenshot rehydration helpers:\n" + "\n".join(rehydrate_lines[-8:]))
    if browser_lines:
        parts.append("Recent trace/output artifacts:\n" + "\n".join(browser_lines[-12:]))
    if status_lines:
        parts.append("Recent status/error events:\n" + "\n".join(status_lines[-8:]))
    return "\n\n".join(parts)


def _message_text(message: Dict[str, Any]) -> str:
    content = message.get("content", "")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for item in content:
            if isinstance(item, dict):
                if item.get("type") == "input_text":
                    parts.append(str(item.get("text") or ""))
                elif item.get("type") == "input_image":
                    parts.append("[input_image]")
            else:
                parts.append(str(item))
        return "\n".join(parts)
    return str(content)


def _trim_message(message: Dict[str, Any]) -> Dict[str, Any]:
    trimmed = deepcopy(message)
    content = trimmed.get("content")
    if isinstance(content, str):
        trimmed["content"] = _compact_text(content, MAX_KEPT_TEXT_CHARS)
    elif isinstance(content, list):
        next_content = []
        for item in content:
            if isinstance(item, dict) and item.get("type") == "input_text":
                next_item = dict(item)
                next_item["text"] = _compact_text(str(next_item.get("text") or ""), MAX_KEPT_TEXT_CHARS)
                next_content.append(next_item)
            else:
                next_content.append(item)
        trimmed["content"] = next_content
    return trimmed


def _compact_text(text: str, max_chars: int) -> str:
    if len(text) <= max_chars:
        return text
    head = max_chars // 2
    tail = max_chars - head
    omitted = len(text) - max_chars
    return f"{text[:head]}\n\n[... omitted {omitted} chars during compaction ...]\n\n{text[-tail:]}"


def _extract_paths(text: str) -> List[str]:
    pattern = re.compile(r"(/[^\s\]\)\"']+\.(?:txt|json|jsonl|png|jpg|jpeg|webp|pdf|csv|tsv|xlsx|html|md|docx))")
    return [match.group(1) for match in pattern.finditer(text)]
