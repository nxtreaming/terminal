from __future__ import annotations

import json
import os
import uuid
from pathlib import Path
from typing import Dict, List, Optional

from llm_browser.events.bus import EventBus
from llm_browser.events.event import Event
from llm_browser.events.store import EventStore
from llm_browser.session.metadata import SessionMetadata


class SessionStore:
    def __init__(self, state_dir: Path, bus: Optional[EventBus] = None) -> None:
        self.state_dir = state_dir
        self.sessions_dir = self.state_dir / "sessions"
        self.events = EventStore(state_dir)
        self.bus = bus or EventBus()

    def create(self, parent_id: Optional[str] = None, cwd: Optional[Path] = None) -> SessionMetadata:
        session_id = uuid.uuid4().hex[:12]
        session = SessionMetadata.create(
            session_id=session_id,
            parent_id=parent_id,
            state_dir=self.state_dir,
            cwd=(cwd or Path.cwd()).resolve(),
        )
        self._write_metadata(session)
        session.artifact_dir.mkdir(parents=True, exist_ok=True)
        self.emit(session.id, "session.created", session.to_dict())
        return session

    def load(self, session_id: str) -> Optional[SessionMetadata]:
        path = self._metadata_path(session_id)
        if not path.exists():
            return None
        with path.open("r", encoding="utf-8") as fh:
            return SessionMetadata.from_dict(json.load(fh))

    def list(self) -> List[SessionMetadata]:
        if not self.sessions_dir.exists():
            return []
        sessions: List[SessionMetadata] = []
        for entry in sorted(self.sessions_dir.iterdir(), key=lambda path: path.name):
            if not entry.is_dir():
                continue
            session = self.load(entry.name)
            if session is not None:
                sessions.append(session)
        return sorted(sessions, key=lambda session: session.updated_ms, reverse=True)

    def update_status(self, session_id: str, status: str) -> SessionMetadata:
        session = self.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")
        updated = session.with_status(status)
        self._write_metadata(updated)
        self.emit(session_id, "session.status", {"status": status})
        return updated

    def request_cancel(self, session_id: str, reason: str = "user requested cancellation") -> None:
        session = self.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")
        payload = {"reason": reason}
        path = self._cancel_path(session_id)
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = path.with_suffix(".json.tmp")
        with tmp_path.open("w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2)
            fh.write("\n")
        os.replace(tmp_path, path)
        self.emit(session_id, "session.cancel_requested", payload)

    def clear_cancel(self, session_id: str) -> None:
        path = self._cancel_path(session_id)
        if path.exists():
            path.unlink()

    def cancel_request(self, session_id: str) -> Optional[Dict[str, str]]:
        path = self._cancel_path(session_id)
        if not path.exists():
            return None
        try:
            with path.open("r", encoding="utf-8") as fh:
                data = json.load(fh)
        except (OSError, json.JSONDecodeError):
            return {"reason": "cancel requested"}
        return {"reason": str(data.get("reason") or "cancel requested")}

    def is_cancel_requested(self, session_id: str) -> bool:
        return self.cancel_request(session_id) is not None

    def emit(self, session_id: str, event_type: str, payload: Optional[dict] = None) -> Event:
        event = Event(type=event_type, session_id=session_id, payload=payload or {})
        self.events.append(event)
        self.bus.publish(event)
        return event

    def _metadata_path(self, session_id: str) -> Path:
        return self.sessions_dir / session_id / "session.json"

    def _cancel_path(self, session_id: str) -> Path:
        return self.sessions_dir / session_id / "cancel.json"

    def _write_metadata(self, session: SessionMetadata) -> None:
        path = self._metadata_path(session.id)
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = path.with_suffix(".json.tmp")
        with tmp_path.open("w", encoding="utf-8") as fh:
            json.dump(session.to_dict(), fh, indent=2)
            fh.write("\n")
        os.replace(tmp_path, path)
