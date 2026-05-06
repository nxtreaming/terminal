from __future__ import annotations

import tempfile
import time
import unittest
from pathlib import Path

from llm_browser.session.store import SessionStore
from llm_browser.tool.context import ToolContext
from llm_browser.tool.files import apply_patch_file, edit_file, glob_files, grep_files, read_file, write_file
from llm_browser.tool.shell import shell, shell_poll, shell_start, shell_stdin, shell_stop


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
            self.assertIn("-world", edited.data["diff"])
            self.assertIn("+browser", edited.data["diff"])
            self.assertIn("browser", read.text)
            self.assertIn("example.txt", globbed.text)
            self.assertIn("browser", grepped.text)

    def test_read_file_supports_line_windows_and_binary_detection(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)
            write_file(ctx, {"path": "lines.txt", "content": "a\nb\nc\n"})
            (ctx.session.cwd / "blob.bin").write_bytes(b"a\x00b")

            lines = read_file(ctx, {"path": "lines.txt", "line_offset": 1, "line_limit": 1})
            binary = read_file(ctx, {"path": "blob.bin"})

            self.assertEqual(lines.text, "b\n")
            self.assertTrue(binary.data["binary"])

    def test_edit_preserves_utf8_bom_and_crlf(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)
            path = ctx.session.cwd / "bom.txt"
            path.write_bytes(b"\xef\xbb\xbffirst\r\nsecond\r\n")

            edit_file(ctx, {"path": "bom.txt", "old": "second", "new": "third"})

            raw = path.read_bytes()
            self.assertTrue(raw.startswith(b"\xef\xbb\xbf"))
            self.assertIn(b"first\r\nthird\r\n", raw)

    def test_shell_runs_in_session_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)

            result = shell(ctx, {"command": "pwd && printf hello", "timeout_s": 5})

            self.assertEqual(result.data["returncode"], 0)
            self.assertIn(str(ctx.session.cwd), result.text)
            self.assertIn("hello", result.text)

    def test_shell_streams_output_events(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)

            result = shell(ctx, {"command": "printf streamed", "timeout_s": 5})

            self.assertEqual(result.data["returncode"], 0)
            output_events = [event for event in ctx.store.events.read(ctx.session.id) if event.type == "tool.output"]
            self.assertTrue(output_events)
            self.assertIn("streamed", output_events[-1].payload["text"])

    def test_shell_timeout_kills_child_process_group(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)
            marker = ctx.session.cwd / "child-survived.txt"
            command = (
                "python - <<'PY'\n"
                "import subprocess, sys, time\n"
                f"subprocess.Popen([sys.executable, '-c', \"import pathlib, time; time.sleep(1); pathlib.Path({str(marker)!r}).write_text('alive')\"])\n"
                "time.sleep(10)\n"
                "PY"
            )

            result = shell(ctx, {"command": command, "timeout_s": 0.2})
            time.sleep(1.2)

            self.assertTrue(result.data["timed_out"])
            self.assertFalse(marker.exists())

    def test_managed_shell_process_can_be_polled_written_and_stopped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)
            command = (
                "python -u -c "
                "\"import sys; print('ready', flush=True); "
                "[print('echo:' + line.strip(), flush=True) for line in sys.stdin]\""
            )

            started = shell_start(ctx, {"command": command, "timeout_s": 10})
            process_id = started.data["process_id"]
            time.sleep(0.2)
            first = shell_poll(ctx, {"process_id": process_id})
            shell_stdin(ctx, {"process_id": process_id, "text": "hello\n"})
            time.sleep(0.2)
            second = shell_poll(ctx, {"process_id": process_id})
            stopped = shell_stop(ctx, {"process_id": process_id})

            self.assertIn("ready", first.text)
            self.assertIn("echo:hello", second.text)
            self.assertTrue(stopped.data["stopped"])

    def test_apply_patch_applies_unified_diff(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_context(tmp)
            write_file(ctx, {"path": "example.txt", "content": "hello\nworld\n"})
            patch = """\
--- a/example.txt
+++ b/example.txt
@@ -1,2 +1,2 @@
 hello
-world
+browser
"""

            result = apply_patch_file(ctx, {"patch": patch})
            read = read_file(ctx, {"path": "example.txt"})

            self.assertEqual(result.data["returncode"], 0)
            self.assertIn("browser", read.text)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
