from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from llm_browser.session.store import SessionStore
from llm_browser.tool.context import ToolContext
from llm_browser.tool.files import edit_file, glob_files, grep_files, read_file, write_file
from llm_browser.tool.shell import shell


class ShellFileToolsTest(unittest.TestCase):
    def make_context(self, tmp: str) -> ToolContext:
        store = SessionStore(Path(tmp) / "state")
        cwd = Path(tmp) / "work"
        cwd.mkdir()
        session = store.create(cwd=cwd)
        return ToolContext(session=session, store=store, tool_call_id="call_1", tool_name="tool")

    def test_file_tools_read_write_edit_glob_grep(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)

            write_file(ctx, {"path": "notes/example.txt", "content": "hello\nworld\n"})
            edited = edit_file(ctx, {"path": "notes/example.txt", "old": "world", "new": "browser"})
            read = read_file(ctx, {"path": "notes/example.txt"})
            globbed = glob_files(ctx, {"root": "notes", "pattern": "*.txt"})
            grepped = grep_files(ctx, {"root": ".", "pattern": "browser", "include": "*.txt"})

            self.assertEqual(edited.data["replacements"], 1)
            self.assertIn("browser", read.text)
            self.assertIn("example.txt", globbed.text)
            self.assertIn("browser", grepped.text)

    def test_shell_runs_in_session_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)

            result = shell(ctx, {"command": "pwd && printf hello", "timeout_s": 5})

            self.assertEqual(result.data["returncode"], 0)
            self.assertIn(str(ctx.session.cwd), result.text)
            self.assertIn("hello", result.text)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
