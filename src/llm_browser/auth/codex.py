from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Optional


@dataclass(frozen=True)
class CodexAuth:
    access_token: str
    account_id: str
    refresh_token: Optional[str]
    id_token: Optional[str]
    source_path: Path
    auth_mode: Optional[str] = None
    last_refresh: Optional[str] = None

    def redacted_summary(self) -> Dict[str, Any]:
        return {
            "available": True,
            "source_path": str(self.source_path),
            "auth_mode": self.auth_mode,
            "account_id": self.account_id,
            "has_access_token": bool(self.access_token),
            "has_refresh_token": bool(self.refresh_token),
            "has_id_token": bool(self.id_token),
            "last_refresh": self.last_refresh,
        }


def load_codex_auth(codex_home: Optional[Path] = None) -> Optional[CodexAuth]:
    home = codex_home or Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")).expanduser()
    path = home / "auth.json"
    if not path.exists():
        return None
    data = json.loads(path.read_text(encoding="utf-8"))
    tokens = data.get("tokens") or {}
    access_token = tokens.get("access_token")
    account_id = tokens.get("account_id")
    if not access_token or not account_id:
        return None
    return CodexAuth(
        access_token=str(access_token),
        account_id=str(account_id),
        refresh_token=_optional_str(tokens.get("refresh_token")),
        id_token=_optional_str(tokens.get("id_token")),
        source_path=path,
        auth_mode=_optional_str(data.get("auth_mode")),
        last_refresh=_optional_str(data.get("last_refresh")),
    )


def _optional_str(value: Any) -> Optional[str]:
    if value is None:
        return None
    return str(value)
