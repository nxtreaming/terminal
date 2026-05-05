from __future__ import annotations


class SessionCancelled(RuntimeError):
    """Raised when a session cancellation request is observed."""

    def __init__(self, session_id: str, reason: str = "cancel requested") -> None:
        self.session_id = session_id
        self.reason = reason
        super().__init__(f"session {session_id} cancelled: {reason}")
