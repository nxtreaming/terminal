from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Dict, List, Tuple


def message_chars(messages: List[Dict[str, Any]]) -> int:
    return sum(len(_message_text(message)) for message in messages)


def compact_messages(
    messages: List[Dict[str, Any]],
    artifact_dir: Path,
    keep_last: int = 12,
) -> Tuple[List[Dict[str, Any]], Path]:
    if len(messages) <= keep_last + 1:
        return messages, artifact_dir / "compactions" / "noop.json"

    kept = messages[-keep_last:]
    summary = _summary(messages[:-keep_last])
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


def _summary(messages: List[Dict[str, Any]]) -> str:
    first_user = ""
    tool_refs = []
    errors = []
    for message in messages:
        role = message.get("role")
        text = _message_text(message)
        if role == "user" and not first_user:
            first_user = text[:3000]
        if role == "tool":
            if "output_path" in text or "artifact" in text or "screenshots" in text:
                tool_refs.append(text[:1200])
            if "tool error" in text or "'ok': False" in text or '"ok": false' in text:
                errors.append(text[:1200])
    parts = []
    if first_user:
        parts.append(f"Original user/task goal:\n{first_user}")
    if tool_refs:
        parts.append("Important tool/artifact references:\n" + "\n\n".join(tool_refs[-8:]))
    if errors:
        parts.append("Recent recoverable errors:\n" + "\n\n".join(errors[-5:]))
    if not parts:
        parts.append(f"Compacted {len(messages)} older message(s). Continue from recent context.")
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
