from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from llm_browser.auth import load_codex_auth


class CodexAuthTest(unittest.TestCase):
    def test_loads_codex_auth_without_exposing_tokens_in_summary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / "auth.json").write_text(
                json.dumps(
                    {
                        "auth_mode": "chatgpt",
                        "tokens": {
                            "access_token": "access-secret",
                            "refresh_token": "refresh-secret",
                            "id_token": "id-secret",
                            "account_id": "acct_123",
                        },
                        "last_refresh": "2026-05-06T00:00:00Z",
                    }
                ),
                encoding="utf-8",
            )

            auth = load_codex_auth(home)

            self.assertIsNotNone(auth)
            assert auth is not None
            self.assertEqual(auth.access_token, "access-secret")
            summary = auth.redacted_summary()
            self.assertEqual(summary["account_id"], "acct_123")
            self.assertTrue(summary["has_access_token"])
            self.assertNotIn("access-secret", json.dumps(summary))
            self.assertNotIn("refresh-secret", json.dumps(summary))

    def test_missing_auth_returns_none(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self.assertIsNone(load_codex_auth(Path(tmp)))


if __name__ == "__main__":
    raise SystemExit(unittest.main())
