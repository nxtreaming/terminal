from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Dict, List

from llm_browser.session.store import SessionStore


def build_trace_bundle(store: SessionStore, session_id: str, max_events: int = 300) -> Dict[str, Any]:
    session = store.load(session_id)
    if session is None:
        raise KeyError(f"session not found: {session_id}")
    events = store.events.read(session_id)
    artifacts = []
    if session.artifact_dir.exists():
        for path in sorted(session.artifact_dir.rglob("*")):
            if path.is_file() and "chrome-profile" not in path.relative_to(session.artifact_dir).parts:
                artifacts.append({"path": str(path), "bytes": path.stat().st_size})
    return {
        "session": session.to_dict(),
        "events": [event.to_dict() for event in events[-max_events:]],
        "event_count": len(events),
        "artifacts": artifacts[-300:],
    }


def write_trace_bundle(store: SessionStore, session_id: str) -> Path:
    session = store.load(session_id)
    if session is None:
        raise KeyError(f"session not found: {session_id}")
    bundle = build_trace_bundle(store, session_id)
    path = session.artifact_dir / "trace-bundle.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(bundle, indent=2) + "\n", encoding="utf-8")
    return path


def build_self_eval_prompt(store: SessionStore, session_id: str) -> str:
    bundle_path = write_trace_bundle(store, session_id)
    bundle = build_trace_bundle(store, session_id, max_events=120)
    compact = {
        "session": bundle["session"],
        "event_count": bundle["event_count"],
        "recent_events": bundle["events"],
        "artifacts": bundle["artifacts"],
        "trace_bundle_path": str(bundle_path),
    }
    return (
        "Evaluate this browser-use-terminal session trace. "
        "Decide whether the original task appears completed, list evidence, identify likely failures, "
        "and propose the next concrete retry/fix if it failed. Be strict.\n\n"
        f"Trace bundle JSON:\n{json.dumps(compact, indent=2)}"
    )
