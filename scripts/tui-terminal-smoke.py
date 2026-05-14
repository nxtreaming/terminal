#!/usr/bin/env python3
"""Real-terminal smoke tests for the Rust TUI.

This intentionally tests the app through tmux instead of Ratatui's TestBackend.
The goal is to catch bugs that only appear with a live terminal viewport:
duplicated panels in scrollback, broken bracketed paste, and stale redraws.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ARTIFACT_DIR = Path("/tmp/but-design-loop")


def run(cmd: list[str], *, check: bool = True, text: str | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=ROOT,
        input=text,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=check,
    )


def tmux(*args: str, check: bool = True) -> str:
    return run(["tmux", *args], check=check).stdout


def tmux_send(session: str, *keys: str) -> None:
    run(["tmux", "send-keys", "-t", session, *keys])


def tmux_send_literal(session: str, value: str) -> None:
    run(["tmux", "send-keys", "-t", session, "-l", value])


def tmux_send_shift_enter(session: str) -> None:
    # Crossterm decodes the kitty/CSI-u enhanced keyboard encoding that the
    # TUI enables at startup. tmux's symbolic "S-Enter" is not reliable across
    # terminal builds and can arrive as plain Enter.
    tmux_send_literal(session, "\x1b[13;2u")


def capture(session: str, name: str) -> str:
    text = tmux("capture-pane", "-t", session, "-p", "-S", "-200")
    ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
    (ARTIFACT_DIR / f"tui-terminal-smoke-{name}.txt").write_text(text)
    return text


def capture_visible(session: str, name: str) -> str:
    text = tmux("capture-pane", "-t", session, "-p")
    ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
    (ARTIFACT_DIR / f"tui-terminal-smoke-{name}.txt").write_text(text)
    return text


def wait_for(session: str, needle: str, name: str, timeout: float = 8.0) -> str:
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        last = capture(session, name)
        if needle in last:
            return last
        time.sleep(0.2)
    raise AssertionError(f"timed out waiting for {needle!r}\n\n{last}")


def capture_after_idle(session: str, name: str, delay: float = 0.5, *, visible_only: bool = False) -> str:
    time.sleep(delay)
    if visible_only:
        return capture_visible(session, name)
    return capture(session, name)


def assert_contains(text: str, needle: str, context: str) -> None:
    if needle not in text:
        raise AssertionError(f"{context}: expected {needle!r}\n\n{text}")


def assert_not_contains(text: str, needle: str, context: str) -> None:
    if needle in text:
        raise AssertionError(f"{context}: unexpected {needle!r}\n\n{text}")


def assert_count(text: str, needle: str, expected: int, context: str) -> None:
    count = text.count(needle)
    if count != expected:
        raise AssertionError(f"{context}: expected {expected} x {needle!r}, saw {count}\n\n{text}")


def assert_row_gap_at_most(text: str, before: str, after: str, max_rows: int, context: str) -> None:
    lines = text.splitlines()
    before_indexes = [idx for idx, line in enumerate(lines) if before in line]
    if not before_indexes:
        raise AssertionError(f"{context}: missing {before!r}\n\n{text}")
    before_idx = before_indexes[-1]
    after_idx = next((idx for idx in range(before_idx + 1, len(lines)) if after in lines[idx]), None)
    if after_idx is None:
        raise AssertionError(f"{context}: missing {after!r} after {before!r}\n\n{text}")
    rows_between = after_idx - before_idx - 1
    if rows_between > max_rows:
        raise AssertionError(
            f"{context}: expected at most {max_rows} rows between {before!r} and {after!r}, saw {rows_between}\n\n{text}"
        )


def assert_row_near_bottom(text: str, needle: str, max_rows_from_bottom: int, context: str) -> None:
    lines = text.splitlines()
    indexes = [idx for idx, line in enumerate(lines) if needle in line]
    if not indexes:
        raise AssertionError(f"{context}: missing {needle!r}\n\n{text}")
    rows_from_bottom = len(lines) - indexes[-1] - 1
    if rows_from_bottom > max_rows_from_bottom:
        raise AssertionError(
            f"{context}: expected {needle!r} within {max_rows_from_bottom} rows of bottom, saw {rows_from_bottom}\n\n{text}"
        )


def assert_regex_count(text: str, pattern: str, expected: int, context: str) -> None:
    count = len(re.findall(pattern, text, flags=re.MULTILINE))
    if count != expected:
        raise AssertionError(f"{context}: expected {expected} x /{pattern}/, saw {count}\n\n{text}")


def assert_no_ansi(text: str, context: str) -> None:
    if re.search(r"\x1b\[[0-?]*[ -/]*[@-~]", text):
        raise AssertionError(f"{context}: output contained ANSI escapes\n\n{text!r}")


def build_binary() -> Path:
    run(["cargo", "build", "-q", "-p", "browser-use-tui", "--bin", "but"])
    binary = ROOT / "target" / "debug" / "but"
    if not binary.exists():
        raise AssertionError(f"missing built binary: {binary}")
    return binary


def start_session(
    session: str,
    binary: Path,
    state_dir: Path,
    *,
    seed_demo: str = "running",
    select_latest: bool = True,
) -> None:
    tmux("kill-session", "-t", session, check=False)
    tmux("new-session", "-d", "-s", session, "-x", "120", "-y", "28")
    tmux("resize-window", "-t", session, "-x", "120", "-y", "28")
    select_arg = "--select-latest " if select_latest else ""
    command = (
        f"cd {ROOT} && {binary} "
        f"--state-dir {state_dir} --seed-demo {seed_demo} {select_arg}--agent none --height 28"
    )
    tmux_send(session, command, "C-m")
    wait_for(session, "Browser Use cloud", f"initial-{seed_demo}")


def smoke_interactive_terminal(binary: Path) -> None:
    session = f"but-smoke-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-"))
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Working (", "initial-running")

        tmux_send(session, "Tab", "Down", "Down", "Down")
        history = wait_for(session, "browser-use / previous work", "history")
        assert_count(history, "browser-use / previous work", 1, "history should be live, not appended repeatedly")
        assert_not_contains(history, "^[[B", "arrow keys should be consumed by the TUI")

        tmux_send(session, "Escape")
        wait_for(session, "Working (", "main-after-history")

        tmux_send_literal(session, "alpha")
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "beta")
        multiline = wait_for(session, "beta", "shift-enter-newline")
        assert_contains(multiline, "> alpha", "multiline input first line")
        assert_contains(multiline, "  beta", "multiline input second line")
        assert_not_contains(multiline, "Follow-up\n    alpha", "shift-enter must not submit")
        assert_not_contains(multiline, "alpha|", "composer should use the terminal cursor, not a fake pipe")
        assert_not_contains(multiline, "beta|", "composer should use the terminal cursor, not a fake pipe")
        assert_count(multiline, "Browser Use cloud", 1, "multiline edit should not append duplicate app screens")

        tmux_send(session, "C-u", "C-u")
        line_removed = capture_after_idle(session, "ctrl-u-removes-empty-composer-line", visible_only=True)
        assert_contains(line_removed, "> alpha", "ctrl-u should keep the previous composer line")
        assert_not_contains(line_removed, "  beta", "second ctrl-u should remove the cleared composer line")

        tmux_send(session, "C-c")
        wait_for(session, "Working (", "main-after-clear")

        bracketed = "\x1b[200~paste one\npaste two\x1b[201~"
        tmux_send_literal(session, bracketed)
        pasted = wait_for(session, "paste two", "bracketed-paste")
        assert_contains(pasted, "paste one", "bracketed paste first line")
        assert_contains(pasted, "paste two", "bracketed paste second line")
        assert_not_contains(pasted, "^[[200~", "bracketed paste markers should not leak")
        assert_not_contains(pasted, "paste two|", "paste should use the terminal cursor, not a fake pipe")
        assert_count(pasted, "Browser Use cloud", 1, "paste should not append duplicate app screens")

        tmux_send(session, "C-c")
        after_paste_clear = capture_after_idle(session, "main-after-paste-clear", visible_only=True)
        assert_contains(after_paste_clear, "Working (", "clearing pasted text should not stop the task")
        assert_not_contains(after_paste_clear, "paste two", "ctrl+c should clear pasted composer text")

        tmux_send(session, "/")
        wait_for(session, "/task", "slash-palette-open")
        wait_for(session, "/model", "slash-palette-open-model")
        tmux_send_literal(session, "bro")
        actions = wait_for(session, "> /bro", "slash-palette-filtered")
        assert_contains(actions, "/browser", "slash palette should show matching command")
        assert_not_contains(actions, "/model", "slash palette should hide non-matching commands")

        tmux_send(session, "Escape")
        wait_for(session, "Working (", "main-after-slash-palette")
        tmux_send(session, "F2")
        browser = wait_for(session, "Current browser", "browser-panel")
        assert_count(browser, "browser-use / browser", 1, "browser panel should be live, not appended repeatedly")

        tmux("resize-window", "-t", session, "-x", "100", "-y", "22")
        resized_small = capture_after_idle(session, "resize-100x22", visible_only=True)
        assert_contains(resized_small, "Current browser", "resize should keep the live app visible")
        assert_regex_count(resized_small, r"^\s+browser-use / browser\b", 1, "resize shrink should redraw in place")
        assert_not_contains(resized_small, "^[[", "resize shrink should not leak escape sequences")

        tmux("resize-window", "-t", session, "-x", "120", "-y", "28")
        resized_large = capture_after_idle(session, "resize-120x28", visible_only=True)
        assert_contains(resized_large, "Current browser", "resize grow should keep the live app visible")
        assert_regex_count(resized_large, r"^\s+browser-use / browser\b", 1, "resize grow should redraw in place")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_history_selection_emits_native_transcript(binary: Path) -> None:
    session = f"but-smoke-history-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-history-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="cancelled",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "history-start-ready")
        tmux_send(session, "Tab")
        wait_for(session, "browser-use / previous work", "history-open-cancelled")
        tmux_send(session, "Enter")
        selected = wait_for(session, "stopped", "history-select-cancelled")
        assert_contains(selected, "> Find the top 5 Hacker News posts", "selected task should be in native scrollback")
        assert_contains(selected, "stopped", "cancelled task should render as native transcript")
        assert_not_contains(selected, "+- stopped", "native transcript should use simple section labels")
        assert_not_contains(selected, "+- browser", "native transcript should use simple section labels")
        assert_row_gap_at_most(
            selected,
            "Progress is saved",
            "Previous work",
            5,
            "stopped status and next menu should stay grouped together",
        )
        assert_row_gap_at_most(
            selected,
            "Previous work",
            "Ask a follow-up",
            1,
            "stopped next menu should stay attached to the composer",
        )
        assert_not_contains(selected, "\x1b[", "native transcript should not leak escapes")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_tall_terminal_keeps_running_controls_attached_to_content(binary: Path) -> None:
    session = f"but-smoke-height-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-height-"))
    try:
        tmux("kill-session", "-t", session, check=False)
        tmux("new-session", "-d", "-s", session, "-x", "120", "-y", "40")
        command = (
            f"cd {ROOT} && {binary} "
            f"--state-dir {state_dir} --seed-demo running --select-latest --agent none"
        )
        tmux_send(session, command, "C-m")
        wait_for(session, "Working (", "height-120x40-history")
        visible = capture_visible(session, "height-120x40")
        assert_count(visible, "Browser Use cloud", 1, "tall terminal should have one live app status")
        assert_row_gap_at_most(
            visible,
            "connected live browser",
            "Type to steer",
            7,
            "running controls should stay attached to the latest activity on tall terminals",
        )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_double_escape_stops_running_task(binary: Path) -> None:
    session = f"but-smoke-esc-stop-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-esc-stop-"))
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Working (", "double-escape-running")
        tmux_send(session, "Escape")
        armed = wait_for(session, "esc again to stop", "double-escape-armed")
        assert_contains(armed, "Working (", "first escape should keep the task running")
        tmux_send(session, "Escape")
        stopped = wait_for(session, "stopped", "double-escape-stopped")
        assert_not_contains(stopped, "^[[", "double escape should not leak escape sequences")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_completed_history_uses_native_scrollback(binary: Path) -> None:
    session = f"but-smoke-long-history-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-long-history-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="long",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "long-history-start-ready")
        tmux_send(session, "Tab", "Enter")
        selected = wait_for(session, "scroll check line 60", "long-history-selected")
        visible = capture_after_idle(session, "long-history-selected-visible", visible_only=True)
        assert_contains(selected, "scroll check line 1", "native transcript should include first result line")
        assert_contains(selected, "scroll check line 60", "native transcript should include last result line")
        assert_contains(selected, "source", "native transcript should include source section")
        assert_contains(visible, "Ask a follow-up", "live viewport should redraw the composer after transcript insert")
        assert_contains(visible, "scroll check line 60", "live viewport should show the native transcript tail")
        assert_contains(visible, "https://news.ycombinator.com", "live viewport should show source above composer")
        assert_row_near_bottom(
            visible,
            "Ask a follow-up",
            5,
            "native scrollback live composer should stay pinned to the terminal bottom",
        )
        assert_not_contains(visible, "scroll check line 1", "live viewport should not echo the completed result")
        assert_row_gap_at_most(
            visible,
            "https://news.ycombinator.com",
            "Ask a follow-up",
            1,
            "native transcript tail should stay attached to the composer",
        )
        assert_not_contains(selected, "+- source", "native transcript should use simple section labels")
        assert_not_contains(selected, "+- result", "native transcript should use simple section labels")
        assert_not_contains(selected, "earlier steps", "native transcript should not compact activity")
        assert_not_contains(selected, "\x1b[", "native transcript should not leak escapes")

        tmux_send_literal(session, "continue")
        tmux_send(session, "Enter")
        running = wait_for(session, "Type to steer the agent", "long-history-followup-running")
        visible_running = capture_after_idle(session, "long-history-followup-visible", visible_only=True)
        if visible_running.count("Browser Use cloud") > 1:
            raise AssertionError(
                "follow-up should not append duplicate app screens\n\n" + visible_running
            )
        assert_not_contains(running, "using browser", "internal browser helper starts should stay hidden")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_short_completed_history_has_live_preview(binary: Path) -> None:
    session = f"but-smoke-short-done-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-short-done-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="done",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "short-done-start-ready")
        tmux_send(session, "Tab", "Enter")
        selected = wait_for(session, "Top 5 Hacker News posts", "short-done-selected")
        visible = capture_after_idle(session, "short-done-selected-visible", visible_only=True)
        assert_contains(selected, "Top 5 Hacker News posts", "selected task should be replayed to native scrollback")
        assert_contains(visible, "Top 5 Hacker News posts", "live viewport should not be blank for completed history")
        assert_contains(visible, "https://news.ycombinator.com", "live viewport should show completed source")
        assert_row_gap_at_most(
            visible,
            "https://news.ycombinator.com",
            "Ask a follow-up",
            1,
            "short completed result should stay attached to the composer",
        )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_main_resize_does_not_duplicate_transcript(binary: Path) -> None:
    session = f"but-smoke-main-resize-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-main-resize-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="long",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "main-resize-start-ready")
        tmux_send(session, "Tab", "Enter")
        wait_for(session, "scroll check line 60", "main-resize-selected")
        tmux("resize-window", "-t", session, "-x", "96", "-y", "24")
        small = capture_after_idle(session, "main-resize-96x24")
        tmux("resize-window", "-t", session, "-x", "140", "-y", "34")
        large = capture_after_idle(session, "main-resize-140x34")
        for name, text in [("small", small), ("large", large)]:
            assert_not_contains(text, "^[[", f"main resize {name} should not leak escapes")
            if text.count("Browser Use cloud") > 1:
                raise AssertionError(
                    f"main resize {name} should not duplicate transcript headers\n\n{text}"
                )
            if text.count("scroll check line 60") > 2:
                raise AssertionError(
                    f"main resize {name} should only replay one transcript plus one live tail preview\n\n{text}"
                )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_session_switch_clears_previous_transcript(binary: Path) -> None:
    session = f"but-smoke-switch-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-switch-"))
    transient_task = "temporary switch task should disappear"
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="long",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "switch-start-ready")
        tmux_send_literal(session, transient_task)
        tmux_send(session, "Enter")
        wait_for(session, transient_task, "switch-transient-created")

        tmux_send(session, "Tab")
        wait_for(session, "browser-use / previous work", "switch-history-open")
        tmux_send(session, "Down", "Enter")
        selected = wait_for(session, "scroll check line 60", "switch-long-selected")
        visible = capture_visible(session, "switch-long-selected-visible")

        assert_contains(selected, "scroll check line 1", "selected transcript should be replayed after switch")
        assert_contains(selected, "scroll check line 60", "selected transcript should include full result after switch")
        assert_not_contains(visible, transient_task, "session switch should clear the previous visible transcript")
        assert_contains(visible, "Ask a follow-up", "session switch should redraw the composer after replay")
        assert_not_contains(visible, "^[[", "session switch clear should not leak escape sequences")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_large_composer_input_is_responsive(binary: Path) -> None:
    session = f"but-smoke-large-input-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-large-input-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="running",
            select_latest=True,
        )
        wait_for(session, "Working (", "large-input-start")
        wait_for(session, "Type to steer the agent", "large-input-composer-ready")
        large_text = "x" * 1200
        started = time.time()
        for offset in range(0, len(large_text), 50):
            tmux_send_literal(session, large_text[offset : offset + 50])
        typed = wait_for(session, "x" * 80, "large-input-typed", timeout=1.5)
        elapsed = time.time() - started
        if elapsed > 1.5:
            raise AssertionError(f"large input took too long to appear: {elapsed:.2f}s")
        assert_not_contains(typed, "^[[200~", "large input should not leak bracketed paste markers")
        assert_not_contains(typed, "^[[", "large input should not leak escape sequences")
        if typed.count("Browser Use cloud") > 1:
            raise AssertionError("large input should not duplicate app screens\n\n" + typed)
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_failed_retry_switches_to_live_running(binary: Path) -> None:
    session = f"but-smoke-failed-retry-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-failed-retry-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="failed",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "failed-retry-start-ready")
        tmux_send(session, "Tab", "Enter")
        wait_for(session, "error", "failed-retry-initial")
        initial = capture_after_idle(session, "failed-retry-initial")
        assert_row_gap_at_most(
            initial,
            "OpenRouter API key is missing",
            "New task",
            6,
            "failed status and next menu should stay grouped together",
        )
        assert_row_gap_at_most(
            initial,
            "New task",
            "Ask a follow-up",
            1,
            "failed next menu should stay attached to the composer",
        )
        tmux_send(session, "Down", "Down", "Enter")
        running = wait_for(session, "Working (", "failed-retry-running")
        visible_running = capture_after_idle(session, "failed-retry-visible", visible_only=True)
        if visible_running.count("Browser Use cloud") > 1:
            raise AssertionError(
                "retry should replace the failure view with one live running viewport\n\n"
                + visible_running
            )
        assert_not_contains(running, "Choose a different model\n    Retry", "retry should not leave the failure action menu live")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_completed_plain_output(binary: Path) -> None:
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-done-"))
    try:
        result = run(
            [
                str(binary),
                "--state-dir",
                str(state_dir),
                "--seed-demo",
                "long",
                "--select-latest",
                "--agent",
                "none",
            ]
        ).stdout
        ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
        (ARTIFACT_DIR / "tui-terminal-smoke-completed-output.txt").write_text(result)
        assert_contains(result, "scroll check line 60", "completed result should print full plain transcript")
        assert_contains(result, "source", "completed result should include source section")
        assert_not_contains(result, "+-", "completed result should use simple section labels")
        assert_no_ansi(result, "completed result should be selectable plain text")
    finally:
        shutil.rmtree(state_dir, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--skip-build", action="store_true", help="reuse target/debug/but")
    args = parser.parse_args()

    if shutil.which("tmux") is None:
        print("tmux is required for real terminal smoke tests", file=sys.stderr)
        return 2

    binary = ROOT / "target" / "debug" / "but" if args.skip_build else build_binary()
    smoke_interactive_terminal(binary)
    smoke_history_selection_emits_native_transcript(binary)
    smoke_tall_terminal_keeps_running_controls_attached_to_content(binary)
    smoke_double_escape_stops_running_task(binary)
    smoke_completed_history_uses_native_scrollback(binary)
    smoke_short_completed_history_has_live_preview(binary)
    smoke_main_resize_does_not_duplicate_transcript(binary)
    smoke_session_switch_clears_previous_transcript(binary)
    smoke_large_composer_input_is_responsive(binary)
    smoke_failed_retry_switches_to_live_running(binary)
    smoke_completed_plain_output(binary)
    print("tui terminal smoke passed")
    print(f"captures: {ARTIFACT_DIR}/tui-terminal-smoke-*.txt")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
