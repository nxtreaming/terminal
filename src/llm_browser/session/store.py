from __future__ import annotations

import json
import os
import socket
import uuid
from pathlib import Path
from typing import Any, Dict, List, Optional

from llm_browser.events.bus import EventBus
from llm_browser.events.event import Event, now_ms
from llm_browser.events.store import EventStore
from llm_browser.session.metadata import SessionMetadata

RUNNABLE_STATUSES = {"created", "running"}


class SessionStore:
    def __init__(self, state_dir: Path, bus: Optional[EventBus] = None) -> None:
        self.state_dir = state_dir.expanduser().resolve()
        self.sessions_dir = self.state_dir / "sessions"
        self.events = EventStore(self.state_dir)
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
        if session.status in {"created", "running"}:
            self.update_status(session_id, "cancelled")

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

    def begin_run(self, session_id: str, label: str = "agent", pid: Optional[int] = None) -> Dict[str, Any]:
        existing = self.runner_info(session_id)
        if existing is not None and self._runner_is_live(existing):
            raise RuntimeError(f"session already has a live runner: {session_id}")

        session = self.load(session_id)
        if session is None:
            raise KeyError(f"session not found: {session_id}")

        runner = {
            "pid": int(pid if pid is not None else os.getpid()),
            "hostname": socket.gethostname(),
            "label": str(label),
            "session_id": session_id,
            "started_ms": now_ms(),
            "cwd": str(session.cwd),
        }
        path = self._runner_path(session_id)
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = path.with_suffix(".json.tmp")
        with tmp_path.open("w", encoding="utf-8") as fh:
            json.dump(runner, fh, indent=2)
            fh.write("\n")
        os.replace(tmp_path, path)
        return runner

    def clear_runner(self, session_id: str, pid: Optional[int] = None) -> bool:
        path = self._runner_path(session_id)
        if not path.exists():
            return False
        if pid is not None:
            info = self.runner_info(session_id)
            if info is not None and int(info.get("pid") or -1) != int(pid):
                return False
        try:
            path.unlink()
        except FileNotFoundError:
            return False
        return True

    def runner_info(self, session_id: str) -> Optional[Dict[str, Any]]:
        path = self._runner_path(session_id)
        if not path.exists():
            return None
        try:
            with path.open("r", encoding="utf-8") as fh:
                data = json.load(fh)
        except (OSError, json.JSONDecodeError):
            return None
        return data if isinstance(data, dict) else None

    def reconcile_orphaned_sessions(self, stale_without_runner_ms: Optional[int] = None) -> List[SessionMetadata]:
        reconciled: List[SessionMetadata] = []
        current_ms = now_ms()
        for session in self.list():
            if session.status not in RUNNABLE_STATUSES:
                continue

            runner = self.runner_info(session.id)
            if runner is not None:
                if self._runner_is_live(runner):
                    continue
                reason = self._stale_runner_reason(runner)
                reconciled.append(self._fail_orphaned_session(session, reason, runner))
                continue

            if stale_without_runner_ms is None:
                continue
            age_ms = current_ms - session.updated_ms
            if age_ms >= max(0, int(stale_without_runner_ms)):
                reason = f"session has been {session.status} for {age_ms}ms with no runner marker"
                reconciled.append(self._fail_orphaned_session(session, reason, None))
        return reconciled

    def emit(self, session_id: str, event_type: str, payload: Optional[dict] = None) -> Event:
        event = Event(type=event_type, session_id=session_id, payload=payload or {})
        self.events.append(event)
        self.bus.publish(event)
        return event

    def _metadata_path(self, session_id: str) -> Path:
        return self.sessions_dir / session_id / "session.json"

    def _cancel_path(self, session_id: str) -> Path:
        return self.sessions_dir / session_id / "cancel.json"

    def _runner_path(self, session_id: str) -> Path:
        return self.sessions_dir / session_id / "runner.json"

    def _write_metadata(self, session: SessionMetadata) -> None:
        path = self._metadata_path(session.id)
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = path.with_suffix(".json.tmp")
        with tmp_path.open("w", encoding="utf-8") as fh:
            json.dump(session.to_dict(), fh, indent=2)
            fh.write("\n")
        os.replace(tmp_path, path)

    def _runner_is_live(self, runner: Dict[str, Any]) -> bool:
        hostname = str(runner.get("hostname") or "")
        if hostname and hostname != socket.gethostname():
            return True
        try:
            pid = int(runner.get("pid") or 0)
        except (TypeError, ValueError):
            return False
        return self._pid_alive(pid)

    def _pid_alive(self, pid: int) -> bool:
        if pid <= 0:
            return False
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        except OSError:
            return False
        return True

    def _stale_runner_reason(self, runner: Dict[str, Any]) -> str:
        pid = runner.get("pid")
        label = runner.get("label") or "agent"
        return f"session runner {label} pid {pid} is no longer alive"

    def _fail_orphaned_session(
        self,
        session: SessionMetadata,
        reason: str,
        runner: Optional[Dict[str, Any]],
    ) -> SessionMetadata:
        payload = {
            "from_status": session.status,
            "to_status": "failed",
            "reason": reason,
            "runner": runner,
        }
        self.emit(session.id, "session.reconciled", payload)
        updated = self.update_status(session.id, "failed")
        self.emit(
            session.id,
            "session.failed",
            {
                "error": reason,
                "error_type": "StaleSession",
                "reconciled": True,
                "runner": runner,
            },
        )
        self.clear_runner(session.id)
        return updated
