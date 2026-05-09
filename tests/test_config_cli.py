from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from llm_browser import cli
from llm_browser.cli import build_parser
from llm_browser.config import apply_config_environment, load_config, redacted_config, write_config_value, write_default_config


class ConfigCliTest(unittest.TestCase):
    def test_load_config_merges_home_and_workspace_configs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home_cfg = root / "home" / ".browser-use-terminal" / "config.json"
            work_cfg = root / "work" / ".browser-use-terminal" / "config.json"
            home_cfg.parent.mkdir(parents=True)
            work_cfg.parent.mkdir(parents=True)
            home_cfg.write_text(json.dumps({"provider": "codex", "browser": {"mode": "chromium", "width": 1100}}), encoding="utf-8")
            work_cfg.write_text(json.dumps({"browser": {"mode": "cloud"}}), encoding="utf-8")

            with patch("llm_browser.config.Path.home", return_value=root / "home"):
                config = load_config(cwd=root / "work")

        self.assertEqual(config["provider"], "codex")
        self.assertEqual(config["browser"]["mode"], "cloud")
        self.assertEqual(config["browser"]["width"], 1100)
        self.assertEqual(len(config["_sources"]), 2)

    def test_parser_uses_config_defaults_but_cli_overrides(self) -> None:
        config = {
            "provider": "codex",
            "model": "gpt-5.5",
            "max_turns": 123,
            "browser": {"mode": "cloud", "headless": True, "width": 1440},
        }

        parser = build_parser(config=config)
        args = parser.parse_args(["run", "open example"])
        override = parser.parse_args(["run", "--provider", "fake", "--browser", "chromium", "open example"])

        self.assertEqual(args.provider, "codex")
        self.assertEqual(args.model, "gpt-5.5")
        self.assertEqual(args.max_turns, 123)
        self.assertEqual(args.browser, "cloud")
        self.assertTrue(args.headless)
        self.assertEqual(args.browser_width, 1440)
        self.assertEqual(override.provider, "fake")
        self.assertEqual(override.browser, "chromium")

    def test_tui_defaults_to_codex_not_fake(self) -> None:
        parser = build_parser(config={})

        args = parser.parse_args(["tui"])

        self.assertEqual(args.provider, "codex")
        self.assertEqual(args.model, "gpt-5.5")

    def test_tui_main_prepends_tui_subcommand_and_keeps_global_args(self) -> None:
        with patch("llm_browser.cli.main", return_value=0) as main:
            self.assertEqual(cli.tui_main(["--state-dir", "/tmp/state", "--browser", "remote"]), 0)

        main.assert_called_once_with(["--state-dir", "/tmp/state", "tui", "--browser", "remote"])

    def test_tui_main_passes_through_top_level_commands(self) -> None:
        with patch("llm_browser.cli.main", return_value=0) as main:
            self.assertEqual(cli.tui_main(["--state-dir", "/tmp/state", "auth", "status"]), 0)

        main.assert_called_once_with(["--state-dir", "/tmp/state", "auth", "status"])

    def test_write_default_config_refuses_overwrite_and_redacts_tokens(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "config.json"
            write_default_config(path)

            with self.assertRaises(FileExistsError):
                write_default_config(path)

        redacted = redacted_config({"auth": {"access_token": "secret"}, "nested": [{"refresh_token": "secret2"}]})
        self.assertEqual(redacted["auth"]["access_token"], "<redacted>")
        self.assertEqual(redacted["nested"][0]["refresh_token"], "<redacted>")

    def test_persisted_browser_use_api_key_redacts_and_hydrates_env(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            path = Path(tmp) / "config.json"
            _, config = write_config_value("browser.cloud_api_key", "bu_secret", path=path)

            apply_config_environment(config)

            self.assertEqual(os.environ["BROWSER_USE_API_KEY"], "bu_secret")
            self.assertEqual(redacted_config(config)["browser"]["cloud_api_key"], "<redacted>")

    def test_openrouter_config_hydrates_env(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            path = Path(tmp) / "config.json"
            _, config = write_config_value("providers.openrouter.api_key", "or_secret", path=path)
            _, config = write_config_value("providers.openrouter.base_url", "https://openrouter.example/api/v1", path=path)

            apply_config_environment(config)

            self.assertEqual(os.environ["LLM_BROWSER_OPENROUTER_API_KEY"], "or_secret")
            self.assertEqual(os.environ["LLM_BROWSER_OPENROUTER_BASE_URL"], "https://openrouter.example/api/v1")
            self.assertEqual(redacted_config(config)["providers"]["openrouter"]["api_key"], "<redacted>")


if __name__ == "__main__":
    raise SystemExit(unittest.main())
