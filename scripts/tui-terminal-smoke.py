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


def tmux_send_shift_letter(session: str, letter: str) -> None:
    if len(letter) != 1 or not letter.isalpha():
        raise ValueError("shift-letter smoke helper expects one alphabetic character")
    tmux_send_literal(session, f"\x1b[{ord(letter.upper())};2u")


def tmux_send_alt_backspace(session: str) -> None:
    # CSI-u Alt+Backspace. This matches the enhanced keyboard protocol enabled
    # by the TUI and avoids relying on tmux's terminal-specific M-BSpace name.
    tmux_send_literal(session, "\x1b[127;3u")


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


def assert_stripped_line(text: str, expected: str, context: str) -> None:
    if not any(line.strip() == expected for line in text.splitlines()):
        raise AssertionError(f"{context}: expected stripped line {expected!r}\n\n{text}")


def assert_no_stripped_line(text: str, expected: str, context: str) -> None:
    if any(line.strip() == expected for line in text.splitlines()):
        raise AssertionError(f"{context}: unexpected stripped line {expected!r}\n\n{text}")


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


def first_text_column(text: str, needle: str, context: str) -> int:
    for line in text.splitlines():
        if needle in line:
            return len(line) - len(line.lstrip())
    raise AssertionError(f"{context}: missing {needle!r}\n\n{text}")


def assert_first_text_columns_close(
    text: str, before: str, after: str, max_delta: int, context: str
) -> None:
    before_column = first_text_column(text, before, context)
    after_column = first_text_column(text, after, context)
    delta = abs(before_column - after_column)
    if delta > max_delta:
        raise AssertionError(
            f"{context}: expected first text columns within {max_delta}, "
            f"saw {before_column} vs {after_column}\n\n{text}"
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


def assert_first_content_near_top(text: str, max_row: int, context: str) -> None:
    for idx, line in enumerate(text.splitlines()):
        if line.strip():
            if idx > max_row:
                raise AssertionError(
                    f"{context}: first visible content should be within {max_row} rows of top, saw row {idx}\n\n{text}"
                )
            return
    raise AssertionError(f"{context}: capture had no visible content\n\n{text}")


def assert_max_consecutive_blank_lines(text: str, max_blank_lines: int, context: str) -> None:
    longest = 0
    current = 0
    for line in text.splitlines():
        if line.strip():
            current = 0
        else:
            current += 1
            longest = max(longest, current)
    if longest > max_blank_lines:
        raise AssertionError(
            f"{context}: expected at most {max_blank_lines} consecutive blank visible lines, saw {longest}\n\n{text}"
        )


def assert_no_ansi(text: str, context: str) -> None:
    if re.search(r"\x1b\[[0-?]*[ -/]*[@-~]", text):
        raise AssertionError(f"{context}: output contained ANSI escapes\n\n{text!r}")


def assert_no_legacy_dashboard_chrome(text: str, context: str) -> None:
    assert_not_contains(text, "[box] Active objective", context)
    assert_not_contains(text, "[box] Task complete", context)
    assert_not_contains(text, "TERMINAL    RUNTIME", context)


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
        f"--state-dir {state_dir} --seed-demo {seed_demo} {select_arg}"
        "--agent none --browser 'Local Chrome' --height 28"
    )
    tmux_send(session, command, "C-m")
    first_visible_text = "Type to steer the agent" if select_latest else "Tell the browser what to do..."
    wait_for(session, first_visible_text, f"initial-{seed_demo}")


def smoke_interactive_terminal(binary: Path) -> None:
    session = f"but-smoke-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-"))
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Type to steer the agent", "initial-running")

        tmux_send(session, "Tab", "Down", "Down", "Down")
        history = wait_for(session, "browser-use / previous work", "history")
        assert_count(history, "browser-use / previous work", 1, "history should be live, not appended repeatedly")
        assert_not_contains(history, "^[[B", "arrow keys should be consumed by the TUI")

        tmux_send(session, "Escape")
        wait_for(session, "Type to steer the agent", "main-after-history")

        tmux_send_literal(session, "alpha")
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "beta")
        multiline = wait_for(session, "beta", "shift-enter-newline")
        assert_contains(multiline, "> alpha", "multiline input first line")
        assert_contains(multiline, "  beta", "multiline input second line")
        assert_not_contains(multiline, "Follow-up\n    alpha", "shift-enter must not submit")
        assert_not_contains(multiline, "alpha|", "composer should use the terminal cursor, not a fake pipe")
        assert_not_contains(multiline, "beta|", "composer should use the terminal cursor, not a fake pipe")
        assert_no_legacy_dashboard_chrome(multiline, "multiline edit should not show old dashboard chrome")
        assert_count(multiline, "Esc:stop", 1, "multiline edit should not append duplicate app screens")

        tmux_send(session, "C-u", "C-u")
        line_removed = capture_after_idle(session, "ctrl-u-removes-empty-composer-line", visible_only=True)
        assert_contains(line_removed, "> alpha", "ctrl-u should keep the previous composer line")
        assert_not_contains(line_removed, "  beta", "second ctrl-u should remove the cleared composer line")

        tmux_send(session, "C-c")
        wait_for(session, "Type to steer the agent", "main-after-clear")

        tmux_send_shift_letter(session, "A")
        shifted = wait_for(session, "> A", "shift-letter-uppercase")
        assert_not_contains(shifted, "> a", "shift-letter input should keep uppercase text")
        tmux_send(session, "C-c")
        wait_for(session, "Type to steer the agent", "main-after-shift-letter-clear")

        tmux_send_literal(session, "/stuff")
        wait_for(session, "> /stuff", "alt-backspace-slash-token-before")
        tmux_send_alt_backspace(session)
        slash_token = capture_after_idle(session, "alt-backspace-slash-token", visible_only=True)
        assert_stripped_line(slash_token, "> /", "alt-backspace should leave slash separator")
        assert_no_stripped_line(slash_token, "> /stuff", "alt-backspace should delete slash word token")
        tmux_send(session, "C-c")
        wait_for(session, "Type to steer the agent", "main-after-alt-backspace-slash-clear")

        tmux_send_literal(session, "something-bla")
        wait_for(session, "> something-bla", "alt-backspace-hyphenated-word-before")
        tmux_send_alt_backspace(session)
        hyphenated_word = capture_after_idle(session, "alt-backspace-hyphenated-word", visible_only=True)
        assert_stripped_line(hyphenated_word, "> something-", "alt-backspace should delete trailing word token")
        assert_no_stripped_line(hyphenated_word, "> something-bla", "alt-backspace should delete trailing word token")
        tmux_send_alt_backspace(session)
        hyphen = capture_after_idle(session, "alt-backspace-hyphen", visible_only=True)
        assert_stripped_line(hyphen, "> something", "alt-backspace should delete punctuation token separately")
        assert_no_stripped_line(hyphen, "> something-", "alt-backspace should delete punctuation token separately")
        tmux_send_alt_backspace(session)
        wait_for(session, "Type to steer the agent", "main-after-alt-backspace-word-clear")

        bracketed = "\x1b[200~paste one\npaste two\x1b[201~"
        tmux_send_literal(session, bracketed)
        pasted = wait_for(session, "paste two", "bracketed-paste")
        assert_contains(pasted, "paste one", "bracketed paste first line")
        assert_contains(pasted, "paste two", "bracketed paste second line")
        assert_not_contains(pasted, "^[[200~", "bracketed paste markers should not leak")
        assert_not_contains(pasted, "paste two|", "paste should use the terminal cursor, not a fake pipe")
        assert_no_legacy_dashboard_chrome(pasted, "paste should not show old dashboard chrome")
        assert_count(pasted, "Esc:stop", 1, "paste should not append duplicate app screens")

        tmux_send(session, "C-c")
        after_paste_clear = capture_after_idle(session, "main-after-paste-clear", visible_only=True)
        assert_contains(after_paste_clear, "Type to steer the agent", "clearing pasted text should not stop the task")
        assert_not_contains(after_paste_clear, "paste two", "ctrl+c should clear pasted composer text")

        tmux_send(session, "/")
        palette = wait_for(session, "/task", "slash-palette-open")
        assert_contains(palette, "/auth", "slash palette should fit every product action")
        assert_contains(palette, "up/down navigate", "slash palette footer should be visible")
        assert_not_contains(palette, "filter actions", "slash palette should not show a redundant filter prompt")
        assert_first_content_near_top(palette, 2, "slash palette should not be pushed down by previous viewport state")
        wait_for(session, "/model", "slash-palette-open-model")
        tmux_send_literal(session, "bro")
        actions = wait_for(session, "> /bro", "slash-palette-filtered")
        assert_contains(actions, "/browser", "slash palette should show matching command")
        assert_not_contains(actions, "/model", "slash palette should hide non-matching commands")

        tmux_send(session, "Escape")
        wait_for(session, "Type to steer the agent", "main-after-slash-palette")
        tmux_send_literal(session, "/model")
        tmux_send(session, "Enter")
        model = wait_for(session, "browser-use setup / model", "model-panel")
        assert_contains(model, "bring your own key", "model surface should show lower sections")
        assert_contains(model, "DeepSeek V4 Pro", "model surface should fit all model rows")
        assert_contains(model, "Enter:select", "model surface footer should be visible")
        assert_first_content_near_top(model, 2, "model surface should not be rendered in the compact dock")
        tmux_send(session, "Escape")
        wait_for(session, "Type to steer the agent", "main-after-model-surface")
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

        for width, height in [(112, 26), (96, 22), (132, 31), (104, 24), (120, 28)]:
            tmux("resize-window", "-t", session, "-x", str(width), "-y", str(height))
        resized_burst = capture_after_idle(session, "resize-burst-120x28", visible_only=True)
        assert_contains(resized_burst, "Current browser", "resize burst should keep the live app visible")
        assert_regex_count(resized_burst, r"^\s+browser-use / browser\b", 1, "resize burst should redraw in place")
        assert_not_contains(resized_burst, "^[[", "resize burst should not leak escape sequences")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_ready_resize_does_not_leave_stale_frames(binary: Path) -> None:
    session = f"but-smoke-ready-resize-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-ready-resize-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="done",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "ready-resize-start")
        for width, height in [(100, 22), (132, 31), (96, 24), (120, 28)]:
            tmux("resize-window", "-t", session, "-x", str(width), "-y", str(height))
        visible = capture_after_idle(session, "ready-resize-visible", visible_only=True)
        full = capture_after_idle(session, "ready-resize-scrollback")
        for name, text in [("visible", visible), ("scrollback", full)]:
            assert_contains(text, "Tell the browser what to do...", f"ready resize {name} should keep composer visible")
            assert_regex_count(text, r"^\s+browser-use\b", 1, f"ready resize {name} should keep one header")
            assert_regex_count(text, r"^\s+\| Browser Use\b", 1, f"ready resize {name} should keep one setup card")
            assert_not_contains(text, "^[[", f"ready resize {name} should not leak escape sequences")
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
        selected = wait_for(session, "Ask a follow-up", "history-select-cancelled")
        assert_contains(selected, "Find the top 5 Hacker", "selected task should be in native scrollback")
        assert_contains(selected, "stopped", "cancelled task should render as native transcript")
        assert_not_contains(selected, "+- stopped", "native transcript should use simple section labels")
        assert_not_contains(selected, "+- browser", "native transcript should use simple section labels")
        assert_row_gap_at_most(
            selected,
            "Progress is saved",
            "Continue with a follow-up",
            5,
            "stopped status and next menu should stay grouped together",
        )
        assert_row_gap_at_most(
            selected,
            "Previous work",
            "Ask a follow-up",
            5,
            "stopped composer should stay attached to the action menu",
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
            f"--state-dir {state_dir} --seed-demo running --select-latest "
            "--agent none --browser 'Local Chrome'"
        )
        tmux_send(session, command, "C-m")
        wait_for(session, "Type to steer the agent", "height-120x40-history")
        visible = capture_visible(session, "height-120x40")
        full = capture_after_idle(session, "height-120x40-scrollback")
        assert_no_legacy_dashboard_chrome(visible, "tall terminal should not show old dashboard chrome")
        assert_contains(
            visible,
            "Reading the page and preparing the next browser action",
            "tall terminal should show streaming answer text",
        )
        assert_count(visible, "Type to steer the agent", 1, "tall terminal should have one live composer")
        assert_row_gap_at_most(
            visible,
            "Reading the page and preparing the next browser action",
            "Type to steer",
            8,
            "running composer should stay attached to sparse running content",
        )
        assert_contains(
            full,
            "Find the top 5 Hacker News posts",
            "running task prompt should be native terminal scrollback",
        )
        assert_not_contains(
            full,
            "waiting for GPT-5.5",
            "transient model wait records should not pollute native scrollback",
        )
        assert_not_contains(
            full,
            ": answer draft",
            "streaming chunks should stay in the live viewport, not permanent scrollback",
        )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_double_escape_stops_running_task(binary: Path) -> None:
    session = f"but-smoke-esc-stop-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-esc-stop-"))
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Type to steer the agent", "double-escape-running")
        tmux_send(session, "Escape")
        armed = wait_for(session, "esc again to stop", "double-escape-armed")
        assert_contains(armed, "Type to steer the agent", "first escape should keep the task running")
        assert_no_legacy_dashboard_chrome(armed, "first escape should not restore old dashboard chrome")
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
        assert_max_consecutive_blank_lines(
            visible,
            8,
            "long completed history should not leave a large blank gap in the visible terminal",
        )
        assert_not_contains(visible, "scroll check line 1", "live viewport should not echo the completed result")
        assert_row_gap_at_most(
            visible,
            "https://news.ycombinator.com",
            "Ask a follow-up",
            3,
            "native scrollback composer should sit directly after the transcript tail",
        )
        assert_not_contains(selected, "+- source", "native transcript should use simple section labels")
        assert_not_contains(selected, "+- result", "native transcript should use simple section labels")
        assert_not_contains(selected, "earlier steps", "native transcript should not compact activity")
        assert_not_contains(selected, "\x1b[", "native transcript should not leak escapes")

        tmux_send_literal(session, "continue")
        tmux_send(session, "Enter")
        running = wait_for(session, "Type to steer the agent", "long-history-followup-running")
        visible_running = capture_after_idle(session, "long-history-followup-visible", visible_only=True)
        if visible_running.count("Type to steer the agent") > 1:
            raise AssertionError(
                "follow-up should not append duplicate app screens\n\n" + visible_running
            )
        assert_no_legacy_dashboard_chrome(visible_running, "follow-up should not show old dashboard chrome")
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
        assert_first_text_columns_close(
            visible,
            "https://news.ycombinator.com",
            "Ask a follow-up",
            1,
            "completed transcript and composer should share a content gutter",
        )
        assert_row_gap_at_most(
            visible,
            "https://news.ycombinator.com",
            "Ask a follow-up",
            3,
            "short completed composer should stay attached to the result",
        )
        tmux_send_literal(session, "alpha")
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "beta")
        multiline = wait_for(session, "beta", "short-done-multiline-followup")
        visible_multiline = capture_after_idle(
            session,
            "short-done-multiline-followup-visible",
            visible_only=True,
        )
        assert_contains(
            multiline,
            "Top 5 Hacker News posts",
            "completed transcript should remain while editing multiline follow-up",
        )
        assert_contains(visible_multiline, "> alpha", "completed follow-up first line")
        assert_contains(visible_multiline, "  beta", "completed follow-up second line")
        assert_not_contains(
            visible_multiline,
            "Follow-up\n    alpha",
            "shift-enter in completed history must not submit",
        )
        assert_no_legacy_dashboard_chrome(
            visible_multiline,
            "completed multiline edit should not show old dashboard chrome",
        )
        tmux_send(session, "C-c")
        wait_for(session, "Ask a follow-up", "short-done-after-multiline-clear")
        tmux_send(session, "/")
        slash = wait_for(session, "/task", "short-done-slash-palette")
        assert_contains(slash, "Top 5 Hacker News posts", "slash palette should not clear completed transcript")
        assert_contains(slash, "/history", "slash palette should open on completed history")
        assert_not_contains(slash, "filter actions", "slash palette should not show a redundant filter prompt")
        assert_first_text_columns_close(
            slash,
            "https://news.ycombinator.com",
            "> /",
            1,
            "slash palette should keep transcript and input aligned",
        )
        assert_row_gap_at_most(
            slash,
            "https://news.ycombinator.com",
            "> /",
            3,
            "slash palette should not push the completed transcript down",
        )
        assert_row_gap_at_most(
            slash,
            "actions",
            "/task",
            3,
            "slash palette should render in-place without a large redraw gap",
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
            assert_contains(text, "> Find the top 5 Hacker News posts", f"main resize {name} should keep transcript visible")
            assert_contains(text, "Ask a follow-up", f"main resize {name} should keep composer visible")
            assert_no_legacy_dashboard_chrome(text, f"main resize {name} should not show old dashboard chrome")
            if text.count("> Find the top 5 Hacker News posts") > 1:
                raise AssertionError(
                    f"main resize {name} should not replay the full transcript more than once\n\n{text}"
                )
            if len(re.findall(r"scroll check line 1$", text, flags=re.MULTILINE)) > 1:
                raise AssertionError(
                    f"main resize {name} should not duplicate the start of the transcript\n\n{text}"
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
        wait_for(session, "temporary switch task sh", "switch-transient-created")

        tmux_send(session, "Tab")
        wait_for(session, "browser-use / previous work", "switch-history-open")
        tmux_send(session, "Down", "Enter")
        selected = wait_for(session, "scroll check line 60", "switch-long-selected")
        visible = wait_for(session, "Ask a follow-up", "switch-long-selected-visible")

        assert_contains(selected, "scroll check line 1", "selected transcript should be replayed after switch")
        assert_contains(selected, "scroll check line 60", "selected transcript should include full result after switch")
        assert_not_contains(visible, transient_task, "session switch should clear the previous visible transcript")
        assert_contains(visible, "Ask a follow-up", "session switch should redraw the composer after replay")
        assert_not_contains(visible, "^[[", "session switch clear should not leak escape sequences")
        assert_first_content_near_top(visible, 2, "selected long transcript should not drift down after switch")
        assert_max_consecutive_blank_lines(
            visible,
            8,
            "selected long transcript should not leave a large blank gap after switch",
        )

        tmux_send(session, "Tab")
        wait_for(session, "browser-use / previous work", "switch-history-reopen-transient")
        tmux_send(session, "Enter")
        transient_visible = wait_for(session, transient_task, "switch-transient-selected-visible")
        assert_first_content_near_top(
            transient_visible,
            2,
            "switching back to another session should reset the inline viewport origin",
        )

        tmux_send(session, "Tab")
        wait_for(session, "browser-use / previous work", "switch-history-reopen-long")
        tmux_send(session, "Down", "Enter")
        long_again = wait_for(session, "scroll check line 60", "switch-long-selected-again-visible")
        assert_first_content_near_top(
            long_again,
            2,
            "switching repeatedly should not accumulate blank rows above transcript",
        )
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
        wait_for(session, "Type to steer the agent", "large-input-start")
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
        if typed.count("Esc:stop") > 1:
            raise AssertionError("large input should not duplicate app screens\n\n" + typed)
        assert_no_legacy_dashboard_chrome(typed, "large input should not show old dashboard chrome")
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
            "Authenticate with OpenRouter",
            6,
            "failed status and next menu should stay grouped together",
        )
        assert_row_gap_at_most(
            initial,
            "Retry",
            "Ask a follow-up",
            6,
            "failed composer should stay attached to the action menu",
        )
        tmux_send(session, "Down", "Down", "Enter")
        running = wait_for(session, "Type to steer the agent", "failed-retry-running")
        visible_running = capture_after_idle(session, "failed-retry-visible", visible_only=True)
        if visible_running.count("Type to steer the agent") > 1:
            raise AssertionError(
                "retry should replace the failure view with one live running viewport\n\n"
                + visible_running
            )
        assert_no_legacy_dashboard_chrome(visible_running, "retry should not show old dashboard chrome")
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
                "--browser",
                "Local Chrome",
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
    smoke_ready_resize_does_not_leave_stale_frames(binary)
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
