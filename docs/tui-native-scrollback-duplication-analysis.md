# TUI Native Scrollback Duplication Analysis

## What Goes Wrong

The TUI currently has two different systems trying to own the same visual content:

- Native terminal scrollback replay prints completed task history into the terminal with `Terminal::insert_before`.
- The Ratatui live viewport then renders a second copy of the same transcript inside the active app frame.

That is why selecting a completed task from history can show the same task output twice: once as real terminal scrollback and once as live app content.

The issue becomes especially obvious for long completed tasks and tasks that used subagents. The screen looks like stale output, duplicated sections, and overlapping UI chrome, even though the underlying event log is mostly sane.

## The Core Problem

The display model does not have a single owner for each layer of the UI.

There should be clear ownership:

- Full completed transcript: native terminal scrollback.
- Current composer and controls: small Ratatui inline viewport.
- History picker: full-screen Ratatui surface.
- Child/subagent summaries: parent `agent.completed` preview, not child `session.done` as another top-level answer.

Instead, the current implementation mixes these modes. After replaying the transcript into terminal scrollback, it still lets the main Ratatui render path build a full transcript view. That makes the UI appear duplicated even when the persisted state contains only one parent final answer.

## Specific Failure Modes

### 1. Native Replay Plus Live Replay

`draw_terminal_frame` calls `maybe_emit_native_transcript` before drawing the Ratatui frame.

That means selected completed history can be inserted into terminal scrollback first. Then `render_main` sees native scrollback as active and still calls `native_replay_live_lines(..., u16::MAX)`, which produces another transcript-sized body for the inline viewport.

Result: the user sees the full transcript twice in one terminal view.

### 2. Child Session Final Answers Leak Into Parent Timeline

The native chronological renderer walks the selected parent session and its child sessions. That part is useful for showing subagent activity, but it currently treats child `session.done` events the same way it treats the parent `session.done`.

For a subagent task, this means:

- the child session emits `session.done` with the helper result;
- the parent emits `agent.completed` with the same helper result payload;
- the parent later emits its own `session.done`.

If all of those are rendered naively, the helper result appears as both a standalone answer and a subagent preview.

Result: subagent summaries look duplicated inside a single task transcript.

### 3. History Is Rendered As A Bottom Pane

`Surface::History` is treated as a bottom pane using the main view. That means opening history does not fully replace the current transcript display. It appears over or below whatever native scrollback was already replayed.

Result: pressing `Tab` can show the history list mixed with previous transcript text, which reads as broken chrome rather than a modal/list view.

### 4. Inline Viewport Is Too Tall For Control-Only Mode

The interactive terminal uses an inline viewport sized close to the whole terminal height. That is useful when Ratatui owns the full screen, but it is wrong after native scrollback owns the transcript.

In native-scrollback mode, the live viewport should be a small bottom control area. If it remains tall, it has enough space to render a duplicate transcript tail or even large chunks of the full task.

## Why Dump Screens Do Not Catch It

`--dump-screen` uses the Ratatui test backend. It does not exercise real terminal scrollback insertion. The bug is caused by the interaction between:

- real terminal inline viewport behavior,
- `Terminal::insert_before`,
- scrollback preservation,
- subsequent Ratatui redraws.

So a deterministic Ratatui dump can look clean while tmux or a real terminal shows duplication.

This is why the repo's terminal testing standard requires tmux smoke tests for TUI changes.

## Desired Architecture

The fix should preserve full history. It should not truncate completed transcripts or hide useful output.

Use two explicit rendering modes:

### Full-Screen Ratatui Mode

Used for:

- setup;
- ready screen;
- history list;
- browser/account/model/developer surfaces;
- active running task views where Ratatui owns the live UI.

In this mode, Ratatui may use the full terminal viewport.

### Native Transcript Mode

Used after selecting a completed, failed, or cancelled task whose transcript should be available as terminal scrollback.

In this mode:

- replay the full transcript once into native terminal scrollback;
- keep only a small Ratatui inline viewport at the bottom;
- render composer, controls, and minimal live status in that viewport;
- do not render the full transcript body again inside Ratatui.

## Concrete Fix Plan

### 1. Stop Rendering Full Transcript In Live Viewport After Native Replay

When `app.native_scrollback_is_active()` is true, `render_main` should not call a transcript-producing body renderer with unlimited height.

Instead, it should render either:

- no body, only composer/footer; or
- a very small status/tail area that cannot duplicate the transcript.

The full transcript has already been inserted into terminal scrollback and should remain selectable there.

### 2. Filter Child Session Terminal Events In Parent Native Replay

When replaying a parent transcript, include child events only as activity, not as full top-level task turns.

Child events to skip in the parent timeline:

- `session.input`
- `session.followup`
- `session.done`
- `session.failed`
- `session.cancelled`

Parent-level `agent.spawned`, `agent.completed`, `agent.failed`, and `agent.cancelled` should remain the source of subagent lifecycle display.

This prevents child final answers from appearing once as standalone answers and again as `agent.completed` previews.

### 3. Make History A Full-Screen Surface

Remove `Surface::History` from the bottom-pane set.

Opening history should temporarily suspend native transcript rendering and show a clean full-screen list. Selecting a task should close history, clear the current app frame if needed, then replay the selected transcript once.

### 4. Separate Terminal Viewport Height By Mode

Use a tall inline viewport only when Ratatui owns the full screen.

When native transcript mode is active, keep the inline viewport small enough for composer/controls. The transcript content should live above it in terminal scrollback.

### 5. Add Regression Coverage For The Real Failure

The smoke test should assert these cases in a real tmux terminal:

- selecting a completed task prints the first and last transcript lines exactly once in scrollback;
- the visible viewport shows the composer and does not duplicate the full transcript;
- selecting a task with a subagent does not show the child `session.done` result as a standalone answer;
- opening history after a selected transcript shows one clean history list, not transcript text plus list text;
- no duplicate app chrome appears after switching sessions, resizing, pressing `Tab`, and returning with `Esc`.

## Code Areas To Change

Likely files:

- `crates/browser-use-tui/src/main.rs`
  - terminal mode selection;
  - native transcript replay;
  - surface handling;
  - inline viewport sizing.

- `crates/browser-use-tui/src/render.rs`
  - `render_main`;
  - native replay live-body behavior;
  - chronological event filtering;
  - history surface behavior.

- `scripts/tui-terminal-smoke.py`
  - add or tighten assertions for real-terminal duplicate transcript behavior.

## Definition Of Done

For this class of fix, `--dump-screen` is not enough.

Required verification:

```bash
scripts/verify-terminal-ui.sh
```

Then inspect `/tmp/but-design-loop/`, especially:

- deterministic dumps for completed/result/history states;
- `tui-terminal-smoke-*.txt`;
- captures around history selection, session switching, resizing, and follow-up submission.

The fix is complete only when the user can:

- select a completed task and scroll/copy the entire transcript from terminal scrollback;
- open history without seeing stale transcript content mixed into the list;
- view subagent summaries once;
- continue with a follow-up from the selected transcript without duplicate app frames.
