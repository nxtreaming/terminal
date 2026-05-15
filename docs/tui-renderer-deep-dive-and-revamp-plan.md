# TUI Renderer Deep Dive And Revamp Plan

This document explains how the current `browser-use-tui` renderer works, why it becomes fragile, how Codex handles the same terminal UI problems, and how to rebuild this renderer around clearer ownership and better performance.

## Part 1: Current Browser-Use TUI Renderer

### Runtime Shape

The TUI is a Ratatui app with two output paths:

- deterministic dumps, used by `--dump-screen`, rendered through Ratatui `TestBackend`;
- real terminal rendering, rendered through Ratatui `CrosstermBackend` with `Viewport::Inline`.

The main binary path is:

1. parse CLI args;
2. create `App`;
3. if `--dump-screen`, call `render_dump`;
4. otherwise, if selected state is already completed, failed, or cancelled, print a native plain transcript and exit;
5. otherwise start the live terminal event loop.

This means the same logical state can be displayed in three different ways:

- Ratatui test backend;
- native plain transcript print-and-exit;
- live inline terminal with optional native scrollback replay plus Ratatui redraws.

That split is central to the current jank.

### App State

`App` is the root mutable UI state. It owns:

- persistent store handle;
- store notification receiver;
- `AppStateCache`;
- selected session id;
- composer text/cursor;
- current surface;
- selected row;
- setup/auth/model/browser settings;
- status notices;
- agent backend;
- quit/escape timers;
- `NativeHistoryState`.

The renderer does not read SQLite directly. It asks `App` for a projected `WorkbenchState`.

### State Cache

`AppStateCache` keeps:

- all known sessions;
- events by session id;
- last loaded event sequence by session id;
- latest projected `WorkbenchState`;
- projection cache key;
- dirty flag.

The cache is updated from two mechanisms:

- store notifications from the writer thread;
- periodic fallback refresh every 750 ms.

On new events, it only loads events after the last seen sequence. On session changes, it refreshes the relevant session and may prune deleted sessions. Whenever sessions or events change, it marks the projection dirty.

### Projection

`refresh_cached_projection` builds a `ProjectionKey` from:

- selected session id;
- browser label;
- whether history tasks are visible.

Then `project_if_needed` calls `project_workbench` from `browser-use-protocol`.

The projection inputs are:

- `current_events`: events for the selected session only;
- `all_events`: either all sessions for history mode, selected session plus descendants, or none;
- all session metadata;
- selected session id;
- browser label.

Important: for selected tasks, `all_events` includes child sessions. That is useful for subagent summaries, but it also means renderer code can accidentally treat child session terminal events as parent transcript events.

### Product State

The UI derives a coarse `ProductState`:

- `SetupNeeded`: no setup, no history, no current session, empty composer;
- `Ready`: no selected current session;
- `Running`: selected session status is active;
- `Cancelled`: selected session status is cancelled;
- `Failed`: projection has failure;
- `Result`: selected session is inactive and not failed/cancelled.

Most layout and keyboard behavior branches on this state.

### Surfaces

Surfaces are:

- `Main`;
- `Setup`;
- `Account`;
- `ApiKey`;
- `Telemetry`;
- `Model`;
- `Browser`;
- `BrowserSelect`;
- `History`;
- `Developer`.

Each surface answers two questions:

- does it use the main view?
- is it a bottom pane?

Currently `Browser`, `BrowserSelect`, `Developer`, `History`, and `Model` are bottom panes. Bottom panes are rendered inside the main layout instead of replacing the whole screen.

That is a major design constraint: history is not a real full-screen mode. It is laid out below the main body. If native transcript scrollback has already been replayed, the history pane can appear mixed with transcript content.

### Render Entry Point

`render(frame, app)` does this:

1. shrink the frame by two columns horizontally;
2. get `WorkbenchState`;
3. derive `ProductState`;
4. if first-run setup should be shown, render setup surface;
5. otherwise, if the surface uses the main view, call `render_main`;
6. otherwise call `render_surface`.

Most surfaces currently use `render_main` because bottom panes count as main-view surfaces.

### Main Layout

`render_main` builds three vertical zones:

- body;
- bottom area;
- footer.

It computes `bottom_h` first:

- normal main surface: composer pane height;
- bottom pane surface: height based on the number of surface lines, clamped.

Then it chooses body lines:

- bottom pane surface: empty body;
- native scrollback active: `native_replay_live_lines`;
- setup ready/running/result/failed/cancelled: corresponding body builder.

After body lines are built, `main_layout_areas` computes heights:

- footer is one row only for bottom panes, quit hint, or pending escape-stop hint;
- body gets either full remaining height if pinned, or `min(body_len, max_body_h)`;
- bottom and footer follow.

`should_pin_main_bottom` currently always returns false. So the body area is content-sized, not bottom-pinned, even for states where the composer should feel fixed.

If body has more lines than body area height, the renderer keeps only the tail.

Finally:

- body is rendered as a single wrapped `Paragraph`;
- if bottom-pane surface, bottom area renders that pane;
- otherwise bottom area renders the composer;
- footer renders only if `show_footer`.

### Full Surface Rendering

`render_surface` is used for non-main surfaces. It clears the full frame, creates:

- optional two-row header;
- body;
- one-row footer.

It then uses `surface_lines` to produce body lines. This path is simpler and cleaner than bottom-pane rendering because it owns the full screen.

### Header

The header is two rows:

- left title;
- right browser/model status;
- horizontal rule.

The browser label is normalized so "not connected" can appear as "ready" or "connected" depending on browser backend.

### Composer

The composer pane is built manually:

- top ASCII border row;
- input rows;
- bottom ASCII border row;
- action row or slash palette.

Input height is based on visual wrapped lines, clamped from 1 to 10. The input width subtracts four columns for margins.

The composer itself stores text as a string plus a character cursor. It supports:

- paste normalization;
- multiline input;
- word deletion;
- line navigation;
- wrapped cursor positioning.

Rendering uses `render_lines_wrapped`, and cursor placement uses `frame.set_cursor_position`.

### Slash Palette

Slash commands are not a separate surface. They are active when the main composer input matches slash syntax.

When active:

- composer pane gets taller;
- action area renders palette lines instead of hint row;
- selection moves over filtered palette items;
- Enter executes selected palette action.

The palette is therefore coupled to composer height and main rendering.

### Ready Screen

When there is no selected task, `ready_lines` renders:

- status header;
- config card;
- recent history preview.

The config card is ASCII-art: `+---+` and `| ... |` lines. Recent history shows up to three rows.

This is a full body render, followed by the composer.

### Work Screen

`work_lines` prefers `tool_aware_chronological_lines`. If that returns none, it falls back to `transcript_lines`.

After building the main transcript/activity body, it appends `next_action_lines` for failed/cancelled states.

This means there are already two render models for task content:

- tool-aware chronological event rendering;
- projected transcript rendering.

### Tool-Aware Chronological Rendering

`tool_aware_chronological_lines`:

1. gets the selected current session;
2. loads chronological events for that session plus descendant sessions;
3. starts with the task prompt;
4. iterates events in sequence order;
5. skips the root `session.input`;
6. accumulates `model.thinking_delta` into a pending block;
7. flushes pending thought blocks before other events;
8. dispatches each event to `append_tool_aware_event`;
9. appends final result/error/stopped/streaming text from projected state.

This renderer is event-driven and mostly ignores the projected transcript except for final text.

### Chronological Event Collection

`chronological_events_for_session` starts with root session id, then walks children by scanning session metadata where `parent_id == current_id`. It recursively includes descendants, then flattens all events for those sessions and sorts by sequence.

The renderer does not retain the event's ownership semantics after this. Every event remains tagged with `event.session_id`, but most render functions do not branch on whether it belongs to the root or a child.

That is the root cause of child `session.done` leaking into the parent visual timeline.

### Tool-Aware Event Dispatch

`append_tool_aware_event` maps raw event types to user-facing blocks.

Examples:

- `session.followup`: prompt section;
- `agent.spawned`: subagent started;
- `agent.completed`: subagent finished plus markdown preview;
- `agent.failed`: error;
- `model.tool_call`: intent line for selected tool calls;
- `model.turn.request`: thinking/waiting line;
- `file.list`: list path;
- `file.read`: read path;
- `file.search`: search query and match count;
- `command.started`: command text;
- `command.output`: first few output lines;
- `tool.output_spilled`: saved artifact path;
- `patch.file_changed`: edit summary;
- browser events: browser connected/opened/live view;
- `plan.updated`: plan updated.

Many tool calls are deliberately hidden: `read_file`, `list_files`, `apply_patch`, `done`, etc. Their effects are expected to appear through specific file/command events.

### Native Timeline Event Dispatch

`append_native_timeline_event` is a second event renderer for native scrollback replay.

It overlaps heavily with `append_tool_aware_event`, but differs:

- renders `session.input` and `session.followup` as prompts;
- renders `session.done` directly as answer text;
- renders `session.failed` and `session.cancelled`;
- hides thinking/request events more aggressively;
- ignores `model.stream_delta`;
- otherwise renders many of the same file/command/browser/subagent events.

This duplicate dispatch table is risky because behavior can diverge from the live work renderer.

### Transcript Fallback

`transcript_lines` uses projected transcript turns rather than raw events.

If transcript turns exist, it renders each turn:

- prompt;
- activity;
- streaming text if pending;
- failure or result.

If no transcript turns exist, it renders:

- task prompt;
- grouped activity;
- result/error/stopped block.

This fallback is simpler, but it is less chronological and less tool-aware.

### Native Plain Transcript Fallback

`native_plain_transcript_lines` is another fallback for native output. It uses transcript turns and formats each turn with prompt/activity/result/failure.

It differs from both tool-aware chronological rendering and native timeline event rendering.

### Activity Grouping

Activity strings are grouped by string-prefix heuristics:

- browser;
- status/thinking;
- tool/subagent;
- run;
- edit;
- other step.

This grouping is based on text like `browsing` , `thinking` , `ran` , `read` , `modified` . It is not a typed render model.

That makes it brittle: changing projection text can change visual grouping.

### Markdown Rendering

Markdown output is parsed with `pulldown_cmark`.

`MarkdownWriter` turns markdown into Ratatui `Line`s and `Span`s:

- headings become bold;
- emphasis/strikethrough become muted;
- strong becomes bold;
- links become underlined and append destination on close;
- images become `[image: ...]`;
- block quotes get `>` ;
- code blocks get a `code` label;
- lists track nested numbering/bullets;
- task list markers become `[x]` or `[ ]`.

After parsing, `wrap_markdown_lines` converts styled spans back into plain text per line for wrapping. This loses mixed-style span fidelity on wrapped lines because it reconstructs text and uses the last span's style for the first wrapped segment, then default text style for continuation lines.

So markdown rendering is useful but not robust.

### Text Wrapping

Most wrapping is done with `wrap_plain`, which splits on whitespace. It does not handle:

- wide grapheme clusters;
- terminal cell width;
- ANSI spans;
- long unbroken words;
- preserving per-word styles.

The renderer generally counts Unicode scalar values with `.chars().count()`, not terminal display width.

### Styling

The theme is a tiny set of style functions:

- text;
- muted;
- dim;
- accent;
- border;
- link;
- done;
- running;
- failed;
- thought.

Styles are applied directly at call sites. There is no semantic token layer such as `TranscriptPrompt`, `ToolName`, `SubtleRule`, `FocusedRow`, or `OverlayChrome`.

### Native Scrollback State

`NativeHistoryState` tracks:

- active session id;
- last event sequence inserted;
- last visual group;
- whether to clear before replay.

It is not a scroll model. It only knows how far native replay has emitted into terminal scrollback.

On new task, new task selection, or new empty task, callers use `reset_with_clear`.

On resize, the app clears the terminal and resets native history.

### Native Transcript Emission

Every live terminal draw calls `maybe_emit_native_transcript` before Ratatui draws.

It:

1. gets terminal size and projected state;
2. returns if current surface does not use main view or first-run setup is visible;
3. clears the screen if `clear_before_replay` was set;
4. returns if no selected session;
5. calculates native scrollback width;
6. if native replay is not active for this session:
  - render all chronological event lines after seq 0;
  - if session is inactive and line count fits visible budget, skip native scrollback;
  - otherwise insert lines before the inline viewport;
  - store session id, last sequence, and last group;
7. if native replay is already active:
  - render events after `last_seq`;
  - insert them before the inline viewport;
  - update last sequence/group.

The insertion uses `terminal.insert_before(height, |buf| Paragraph::new(lines).render(...))`.

This is the only path that creates real terminal scrollback while the app is live.

### Native Scrollback Threshold

For inactive sessions, native replay is used only when rendered line count exceeds:

```text
terminal height - 8, with a minimum of 8
```

Running sessions always use native replay once selected because `session_is_active` bypasses the inactive-fit check.

This heuristic causes mode switches based on content height, not user intent.

### Live Inline Viewport

The terminal is created with:

```text
Viewport::Inline(live_height)
```

`live_height` is currently derived from real terminal height:

```text
height - 1, minimum 12
```

So the inline viewport is almost the whole terminal. That is fine if Ratatui owns the screen. It is wrong if native scrollback owns the transcript and Ratatui should only own controls.

### Print-And-Exit Mode

Before running the interactive terminal, `should_print_and_exit` checks if the selected main state is result/failed/cancelled. If yes, it calls `print_native_transcript` and exits.

This makes `cargo run ... --select-latest` print plain transcript and quit instead of opening an interactive view.

The behavior differs from selecting the same task inside the interactive TUI.

### Resize Behavior

Resize events are debounced for 80 ms.

On settled resize:

- clear all;
- purge scrollback;
- move to top-left;
- autoresize terminal;
- clear Ratatui;
- reset native history.

This is heavy-handed but prevents stale inline frames. It also discards native scrollback history, which fights the goal of preserving full transcript history.

### Keyboard Model

The terminal enables:

- raw mode;
- bracketed paste;
- keyboard enhancement flags for disambiguated escape codes, event types, alternate keys.

Key handling is centralized in `App::handle_key`.

Important behavior:

- `Ctrl-Q`: quit;
- `Ctrl-C`: clear composer, cancel task, or double-press quit;
- `Esc`: close palettes/surfaces or arm stop for running task;
- `Tab`: open history;
- `F2`: browser surface;
- `Ctrl-E`: developer surface;
- arrows: either composer movement or selection movement depending on surface/composer state;
- Enter: slash palette selection, setup selection, surface selection, or submit;
- paste: composer/API key insertion depending on surface.

This keyboard model is tightly coupled to surfaces and the composer. There is no separate command routing table.

### Deterministic Dump Rendering

`render_dump`:

- drains notifications;
- creates `TestBackend`;
- calls `render`;
- converts buffer cells to plain text.

It does not exercise:

- real terminal raw mode;
- inline viewport behavior;
- `insert_before`;
- native scrollback;
- resize terminal side effects.

That is why dump output can look fine while tmux output is broken.

## Current Failure Pattern

The duplication bug comes from unclear visual ownership.

When a completed task is selected from history:

1. `SelectHistory` sets `selected_session_id`, resets native history with clear, and closes the surface.
2. next draw calls `maybe_emit_native_transcript`.
3. native transcript lines are inserted into real terminal scrollback.
4. Ratatui then draws the main frame.
5. because native history is active, `render_main` calls `native_replay_live_lines`.
6. that function can render the full transcript again into the live viewport.

So the transcript appears twice.

For subagent tasks, there is another duplicate:

1. chronological event collection includes child session events;
2. child `session.done` is rendered as an answer;
3. parent `agent.completed` renders the same child result as subagent preview;
4. parent `session.done` renders the parent answer.

So helper output appears both as a standalone answer and as subagent output.

## Main Structural Problems

- Multiple renderers own the same transcript.
- Event ownership is flattened too early.
- History is a bottom pane even though it should be a full-screen picker.
- Native scrollback mode is implicit and content-height driven.
- The live inline viewport is almost full screen even when it should be control-only.
- Rendering builds large `Vec<Line>` bodies every frame.
- Wrapping and width calculations use character counts instead of terminal cell width.
- Style and layout are coupled to string-building functions.
- The renderer has duplicate event dispatch tables.
- Deterministic dumps do not cover the most important terminal behavior.

## Part 2: Codex TUI Renderer

This section describes the Codex renderer at `/Users/greg/Downloads/tmp/codex`, based on the code under `codex-rs/tui`.

### Runtime Shape

Codex also uses Ratatui, but it does not use Ratatui terminal output as the whole renderer. It wraps Ratatui in a custom terminal layer:

- `codex-rs/tui/src/tui.rs`: terminal lifecycle, terminal modes, event stream, frame scheduling, viewport resizing, history flushing.
- `codex-rs/tui/src/custom_terminal.rs`: Ratatui-like terminal with explicit buffers, viewport area, cursor state, and diff flushing.
- `codex-rs/tui/src/insert_history.rs`: native terminal scrollback insertion.
- `codex-rs/tui/src/chatwidget.rs`: main chat surface and active in-flight cell.
- `codex-rs/tui/src/history_cell.rs`: typed transcript cell abstraction.
- `codex-rs/tui/src/app/event_dispatch.rs`: converts app events into transcript cells and terminal history inserts.
- `codex-rs/tui/src/app/resize_reflow.rs`: repairs terminal scrollback after resize.
- `codex-rs/tui/src/pager_overlay.rs`: full-screen transcript/static pager overlays.
- `codex-rs/tui/src/bottom_pane/*`: composer, popups, approvals, footer, selection views.

The important architectural point:

> Codex treats finalized transcript history as terminal scrollback, not as a Ratatui body that is redrawn every frame.

The live Ratatui viewport is mostly the active cell plus the bottom pane. Finalized transcript rows are inserted above that viewport.

### Terminal Modes

Startup in `tui::init`:

1. verify stdin/stdout are terminals;
2. enable bracketed paste;
3. enable raw mode;
4. enable keyboard enhancement flags when supported;
5. enable focus-change events;
6. flush pending terminal input;
7. install a panic hook that restores terminal modes;
8. probe the starting cursor position;
9. create `CustomTerminal` using the real cursor position.

Codex tracks whether alternate screen is enabled:

- `--no-alt-screen` disables alternate screen;
- config can set alternate screen `always`, `never`, or `auto`;
- `auto` disables alternate screen in Zellij and enables it elsewhere.

This matters because Codex has two visual regimes:

- normal inline mode, where finalized history is real scrollback above the viewport;
- alternate-screen overlays, where a pager owns the whole terminal.

Overlay entry calls `tui.enter_alt_screen()`. Overlay exit calls `tui.leave_alt_screen()` and restores the saved inline viewport.

### Custom Terminal

`custom_terminal::Terminal<B>` is a forked/specialized terminal driver. It owns:

- backend writer;
- two Ratatui `Buffer`s;
- current-buffer index;
- cursor hidden/shown state;
- `viewport_area`;
- `last_known_screen_size`;
- `last_known_cursor_pos`;
- `visible_history_rows`.

The initial `viewport_area` starts at the probed cursor row:

```text
x = 0
y = cursor_pos.y
width = 0
height = 0
```

The viewport is then resized by `Tui::draw` to the height requested by the app.

The custom terminal differs from a simple Ratatui terminal in several ways:

- it knows the viewport can start below row 0;
- it tracks how many native history rows are visible above the viewport;
- it can clear screen and scrollback with explicit ANSI;
- it can invalidate its diff buffer after out-of-band terminal mutations;
- it uses its own diff-to-crossterm command path;
- it strips OSC sequences before width measurement so hyperlinks do not count as visible columns.

### Frame Drawing

Codex draw flow:

1. app computes `desired_height = chat_widget.desired_height(width)`;
2. app calls `tui.draw(desired_height, |frame| ...)`;
3. `Tui::draw` runs inside `stdout().sync_update(...)`;
4. pending viewport adjustment is applied;
5. inline viewport is resized to `desired_height`;
6. queued history lines are flushed into terminal scrollback above the viewport;
7. custom terminal draws a Ratatui frame into the viewport;
8. cursor position/style is applied after flushing;
9. buffers are swapped.

The invariant is simple:

> terminal scrollback receives finalized history, while the live viewport receives only current interactive UI.

There is no normal path where the same finalized transcript is both inserted into scrollback and also redrawn as the main Ratatui body.

### Buffer Diffing

`custom_terminal::flush` diffs previous/current buffers and writes only changed cells.

The diff path:

1. scan each row for the last meaningful column;
2. emit `ClearToEnd` where trailing cells can be cleared cheaply;
3. compare current and previous cells;
4. handle multi-width glyph invalidation;
5. emit `Put` draw commands for changed visible cells;
6. write commands with minimal cursor moves and style transitions.

The writer tracks:

- foreground color;
- background color;
- modifiers;
- last cursor position.

At the end it resets foreground/background/modifiers.

This is not just polish. It gives Codex control over terminal damage, cursor placement, and repaint cost.

### History Insertion

`insert_history.rs` is the core Codex scrollback trick.

`Tui::insert_history_lines` queues styled Ratatui `Line`s. It does not immediately print them. On the next draw, `flush_pending_history_lines` calls:

```text
insert_history_lines_with_mode_and_wrap_policy(...)
```

That function:

1. reads screen size;
2. reads current viewport area;
3. pre-wraps lines according to the selected wrap policy;
4. calculates how many physical terminal rows the lines need;
5. creates room above the viewport;
6. writes styled lines into that room;
7. restores cursor position;
8. updates viewport area if it was pushed down;
9. increments `visible_history_rows`.

There are two insertion modes:

- `Standard`: uses DECSTBM scroll regions and reverse index to create space above the viewport.
- `Zellij`: emits newlines at the bottom and writes lines at absolute positions because Zellij mishandles the standard scroll-region path.

There are two wrap policies:

- `PreWrap`: Codex wraps before insertion so it controls row count.
- `Terminal`: Codex leaves lines mostly intact, used for raw output mode where clean selection matters.

URL handling is special:

- URL-only lines are kept intact so terminal emulators can detect clickable links;
- mixed URL/prose lines are adaptively wrapped so URL tokens are not split;
- non-URL text is wrapped normally.

This is far more explicit than our current `insert_before` path. Codex treats scrollback insertion as a first-class renderer operation with row accounting.

### Transcript Data Model

The core abstraction is `HistoryCell`.

Each cell can produce:

- `display_lines(width)`: rich lines for main terminal history;
- `raw_lines()`: copy-friendly raw output;
- `display_lines_for_mode(width, mode)`;
- `desired_height(width)`;
- `transcript_lines(width)`: lines for the full transcript overlay;
- `desired_transcript_height(width)`;
- `is_stream_continuation()`;
- `transcript_animation_tick()`;
- `should_render()`;
- downcast access through `as_any`.

The important detail:

> history is not rebuilt from raw events during every render. Events are converted into typed cells, and cells own their own rendering contract.

Examples include:

- user messages;
- assistant markdown;
- reasoning summaries;
- active tool calls;
- completed tool calls;
- exec interactions;
- patch summaries;
- web searches;
- plan updates;
- warnings/errors/info;
- session headers.

Cells are composable. `CompositeHistoryCell` groups multiple cells while preserving the same interface.

### Active Vs Committed History

Codex separates committed transcript from in-flight UI:

- `App.transcript_cells`: committed source of truth for transcript history.
- `ChatWidget.active_cell`: current mutable streaming/tool cell.
- `ChatWidget.active_hook_cell`: optional active hook cell.

When an active cell finalizes, `ChatWidget::flush_active_cell` sends:

```text
AppEvent::InsertHistoryCell(active)
```

`AppEvent::InsertHistoryCell` handling:

1. converts `Box<dyn HistoryCell>` to `Arc<dyn HistoryCell>`;
2. inserts it into transcript overlay if open;
3. pushes it to `App.transcript_cells`;
4. converts it to display lines at the current width;
5. queues those lines for terminal scrollback insertion.

This means a finalized cell has exactly one source object and exactly one terminal insertion path.

### Stream Consolidation

Streaming is handled carefully because transient chunks are bad resize sources.

For assistant markdown:

1. streaming may create multiple `AgentMessageCell`s;
2. finalization emits `ConsolidateAgentMessage { source, cwd }`;
3. `App` finds the trailing run of stream cells;
4. it replaces the run in `transcript_cells` with one `AgentMarkdownCell`;
5. if transcript overlay is open, it mirrors the splice;
6. resize reflow is run if needed.

The same pattern exists for proposed plans.

This is a big robustness lesson: transient display cells are allowed, but the final transcript is source-backed.

### Resize Reflow

Terminal scrollback is not retained UI. Once lines are inserted, the terminal owns the rows. If the terminal width changes, old scrollback wrapping is wrong.

Codex solves this with `TranscriptReflowState` and `app/resize_reflow.rs`.

State tracked:

- last observed width;
- last reflowed width;
- pending target width;
- pending debounce deadline;
- whether a reflow ran during stream;
- whether resize was requested during stream.

On draw/resize:

1. sample terminal size;
2. note width;
3. detect width/height changes;
4. notify `ChatWidget` of terminal width changes;
5. schedule a debounced reflow;
6. clear queued history inserts that were wrapped for the old size;
7. when due, rebuild scrollback from `transcript_cells`.

Reflow rebuild:

1. render transcript cells for the current width;
2. optionally cap retained rows;
3. clear Codex-owned terminal scrollback and visible screen;
4. reset viewport to top;
5. insert rebuilt transcript lines;
6. mark the width as reflowed.

Streaming edge case:

- if resize happens while streaming, Codex marks stream-time reflow state;
- after stream consolidation, Codex forces a final source-backed reflow.

This is exactly the class of robustness our renderer lacks today.

### Main Chat Widget

`ChatWidget` renders the live inline viewport, not the whole transcript.

`ChatWidget::as_renderable` builds a vertical flex layout:

1. active cell area, flex 1;
2. active hook cell area, fixed/optional;
3. bottom pane, fixed.

`ChatWidget::desired_height(width)` asks the renderable tree how many rows it wants.

The bottom pane owns:

- composer;
- status indicator;
- unified exec footer;
- pending input preview;
- pending approval/thread preview;
- popup/view stack.

This is another important distinction:

> Codex does not treat every surface as a branch inside one giant render function. It gives each surface a `Renderable` contract and lets layout compose them.

### Bottom Pane

`BottomPane` is the interactive footer.

It owns:

- `ChatComposer`;
- stack of `BottomPaneView`s;
- delayed approval requests;
- local composer activity timing;
- frame requester;
- keymap;
- status/preview/footer state.

If a view is active, the bottom pane renders the view. Otherwise it renders:

1. status row if present;
2. unified exec footer if no status row owns that content;
3. pending thread approvals;
4. pending input preview;
5. composer.

The bottom pane has its own input-routing contract:

- active view gets keys first;
- composer handles text/history/paste;
- parent `ChatWidget` handles process-level actions such as interrupt/quit.

That separation prevents the common "global key handler knows every detail" problem.

### Full Transcript Overlay

Codex does not make transcript/history a bottom pane.

`Ctrl+T`:

1. enters alternate screen;
2. creates `Overlay::Transcript` from `App.transcript_cells`;
3. renders a full-screen pager;
4. handles scroll/page/jump/close keys inside the overlay;
5. leaves alternate screen on close.

The overlay has:

- committed cells;
- renderables derived from those cells;
- optional highlighted cell;
- optional cached live tail;
- scroll offset;
- content height cache;
- bottom hint area.

Live tail:

- derived from `ChatWidget.active_cell`;
- cached by width, active-cell revision, continuation flag, animation tick;
- recomputed only when the key changes;
- appended after committed cells as render-only content.

This is the right model for our history view. It is a separate full-screen surface, not a footer pane competing with the transcript.

### Renderable Contract

Codex uses a small renderer abstraction:

```text
trait Renderable {
    fn render(&self, area, buf);
    fn desired_height(&self, width) -> u16;
    fn cursor_pos(&self, area) -> Option<(u16, u16)>;
    fn cursor_style(&self, area) -> SetCursorStyle;
}
```

There are reusable layout renderables:

- `ColumnRenderable`;
- `FlexRenderable`;
- `RowRenderable`;
- `InsetRenderable`;
- `CachedRenderable` in pager overlay.

This gives every piece a measurement pass and a render pass. That is missing in our renderer, where many functions produce lines and the layout code guesses how much space they consume.

### Wrapping

Codex centralizes wrapping in:

- `wrapping.rs`;
- `live_wrap.rs`;
- `markdown_render.rs`;
- `render/line_utils.rs`.

Key rules:

- use `textwrap`;
- use Ratatui `Paragraph::line_count` for rendered height;
- use `unicode-width` for terminal cell width;
- preserve URLs when possible;
- use helper functions to prefix lines instead of hand-building string indentation;
- support raw and rich output modes.

The practical lesson:

> terminal text wrapping is not string length. It is display-cell measurement plus source-aware wrapping policy.

### Frame Scheduling

Codex does not redraw whenever anything twitches.

`FrameRequester`:

- is cloneable;
- receives immediate or delayed draw requests;
- sends draw events on a broadcast channel;
- coalesces multiple requests;
- rate-limits to 120 FPS.

Widgets can schedule frames for:

- animations;
- delayed hints;
- paste burst flushing;
- resize debounce;
- overlay scrolling.

This reduces waste and makes animations/timers explicit.

### Styling Discipline

Codex has a TUI style guide:

- headers are bold;
- primary text uses default foreground;
- secondary text is dim;
- selection/status/user tips use cyan;
- success/additions use green;
- errors/deletions use red;
- Codex brand uses magenta;
- avoid custom RGB;
- avoid white/black foreground;
- avoid blue/yellow unless there is a specific reason.

This is not just aesthetic. It makes the UI survive different terminal themes.

### Test Model

Codex has broad renderer tests:

- `insta` snapshots for TUI output;
- snapshots for history cells;
- snapshots for bottom pane components;
- snapshots for diff rendering;
- unit tests for wrapping;
- vt100-style tests for scrollback insertion and ANSI behavior.

For UI changes, their repo instructions require `cargo test -p codex-tui` and snapshot review.

The test lesson:

> test backend snapshots catch layout regressions, but Codex also tests terminal insertion logic with terminal-like backends because scrollback behavior is not represented by plain Ratatui buffers.

## Part 3: How Our Renderer Differs From Codex

### Ownership

Our renderer:

- `render.rs` owns almost every surface;
- transcript lines are derived from events repeatedly;
- native scrollback replay is bolted onto a Ratatui full-screen body;
- history is a bottom pane;
- child session events are flattened into parent transcript rendering.

Codex:

- terminal driver owns viewport and scrollback mechanics;
- `insert_history` owns native scrollback insertion;
- `HistoryCell` owns transcript display;
- `ChatWidget` owns only active/live viewport content;
- `BottomPane` owns interactive footer;
- pager overlays own full-screen transcript/static views;
- app owns committed `transcript_cells`.

### Main Screen Model

Our live screen tries to show:

- header;
- transcript;
- activity;
- surface/body;
- composer;
- footer;
- native replay copy of the transcript.

Codex live screen shows:

- finalized transcript in real terminal scrollback above viewport;
- active cell in the viewport;
- bottom pane in the viewport.

That is the core reason Codex can show entire history without duplicating it: the transcript has one live owner.

### History Surface

Our `Surface::History` is part of bottom pane layout. It competes with the main transcript/body and can expose stale draw state.

Codex transcript overlay is full-screen alternate-screen pager. It has its own scroll state, height cache, keymap, and live tail.

### Event Model

Our renderer builds human output from raw events with large match functions:

- `append_tool_aware_event`;
- `append_native_timeline_event`;
- transcript fallback functions;
- activity grouping functions.

Codex converts events into typed cells. After conversion, rendering works from cell interfaces, not raw event strings.

This is the biggest semantic difference. Our renderer repeats interpretation in multiple places. Codex interprets once, then renders typed objects.

### Native Scrollback

Our native path:

- builds text lines;
- uses `Terminal::insert_before`;
- sets native history state;
- still renders live transcript lines in the Ratatui body.

Codex native path:

- queues history lines;
- flushes them during draw;
- updates viewport position;
- invalidates diff buffer when necessary;
- never draws committed transcript as main body in the same mode.

### Resize

Our renderer has no real source-backed resize reflow. Once the terminal wraps inserted scrollback, that wrapping is whatever the terminal did.

Codex stores committed `HistoryCell`s and rebuilds terminal scrollback after width/height changes.

### Performance

Our renderer does lots of work per frame:

- rebuilds chronological lines;
- scans and sorts events;
- wraps/indents strings;
- constructs large line vectors;
- duplicates logic for native and Ratatui output.

Codex shifts work to event-time and cache-time:

- event -> cell once;
- cell -> lines by width;
- pager caches desired heights;
- frame requests are coalesced;
- resize reflow renders a suffix when row cap applies;
- scrollback insertion batches pending lines.

### Width Correctness

Our code often uses character counts and manual truncation/wrapping.

Codex uses:

- `unicode-width`;
- `textwrap`;
- Ratatui `Paragraph::line_count`;
- URL-aware wrapping;
- OSC-aware display width in terminal diffing.

### Test Coverage

Our deterministic dumps miss the failure mode because they do not exercise real scrollback or inline viewport behavior.

Codex separately tests:

- component snapshots;
- history cell snapshots;
- terminal insertion using vt100-like backends;
- resize reflow state;
- overlay pager logic.

## Part 4: What Actually Goes Wrong In Our Renderer

The broken screenshots are a symptom of one root problem:

> the renderer has no single visual ownership model for transcript history.

More concretely:

1. We want entire completed task history visible.
2. To get that, native scrollback insertion was added.
3. But the existing Ratatui main body still renders the same transcript.
4. The history picker is a bottom pane, so it appears inside the same layout tree instead of replacing it.
5. Child session events are flattened into parent event rendering, so subagent output can be interpreted twice.
6. Native mode is triggered by content height, not by an explicit renderer mode.
7. The live viewport remains large, so Ratatui can redraw stale transcript-like content after scrollback insertion.
8. Dumps do not catch it because dumps only show the Ratatui buffer, not the terminal scrollback side effects.

That is why it looks like "duplicates everywhere". The data is not necessarily duplicated. The visual renderer is duplicating ownership and interpretation.

## Part 5: Renderer Revamp Plan

### Design Goal

Build the renderer around this invariant:

> every piece of visual content has exactly one owner in each renderer mode.

For the normal chat mode:

- committed transcript owner: terminal scrollback;
- active output owner: live viewport active cell;
- input/status owner: dock/bottom pane;
- full history browsing owner: full-screen overlay.

### Target Architecture

Split the renderer into five layers.

#### 1. Transcript Model

Create typed transcript nodes from events:

```text
TranscriptNode
  id: stable event/session/tool id
  owner: Root | Child(session_id)
  visibility: Main | ChildSummary | DebugOnly
  kind:
    UserPrompt
    AssistantMessage
    Reasoning
    ToolCall
    ToolOutput
    ToolGroup
    SubagentStarted
    SubagentCompleted
    BrowserState
    Artifact
    Error
    SystemNotice
```

Rules:

- event interpretation happens once;
- child session terminal output does not become parent top-level answer text;
- parent can show child summary nodes;
- debug surfaces can still expose child internals;
- every node carries stable identity for caching.

#### 2. Render Cells

Convert transcript nodes into `RenderCell`s:

```text
trait RenderCell {
    fn id(&self) -> RenderCellId;
    fn display_lines(&self, width: u16, mode: RenderMode) -> Vec<Line<'static>>;
    fn raw_lines(&self) -> Vec<Line<'static>>;
    fn desired_height(&self, width: u16, mode: RenderMode) -> u16;
    fn is_stream_continuation(&self) -> bool;
}
```

Use cells for:

- user prompt;
- assistant answer;
- tool call group;
- command output;
- file read/list/search;
- browser event;
- subagent status/result;
- artifact/image;
- warning/error;
- session summary.

This removes the parallel `append_tool_aware_event` vs `append_native_timeline_event` problem.

#### 3. Terminal Driver

Replace ad hoc native history with a terminal driver:

```text
TerminalDriver
  viewport_area
  last_screen_size
  last_cursor_pos
  visible_history_rows
  pending_scrollback_lines
  draw_viewport(...)
  insert_scrollback(...)
  clear_owned_scrollback(...)
  invalidate_viewport(...)
```

This can borrow heavily from Codex:

- custom terminal wrapper;
- explicit viewport area;
- pending history queue;
- scroll-region insertion;
- Zellij fallback;
- cursor restoration;
- full diff invalidation after raw terminal mutations.

Do not let normal `render_main` print transcript lines in native-scrollback mode.

#### 4. Live Layout

Normal live layout should be:

```text
Terminal scrollback:
  committed transcript cells

Inline Ratatui viewport:
  active cell or compact latest status
  dock:
    status
    pending approvals
    browser/agent indicators
    composer
    footer hints
```

The viewport height should be measured from active cell + dock, not set to nearly the full terminal height.

#### 5. Overlays

Make these full-screen surfaces:

- history/task picker;
- full transcript;
- browser;
- developer/debug;
- model/provider/account setup if large enough.

History/task picker should not be a bottom pane. It should own the screen while open, with its own scroll offset and keymap.

### Renderer Modes

Make renderer mode explicit:

```text
RendererMode
  LiveInline
  FullscreenOverlay(OverlayKind)
  PrintAndExit
  Dump
```

Each mode has a clear ownership table:


| Mode                | Transcript owner          | Active owner            | Input owner          |
| ------------------- | ------------------------- | ----------------------- | -------------------- |
| `LiveInline`        | terminal scrollback       | viewport active cell    | dock                 |
| `FullscreenOverlay` | overlay pager             | overlay pager/live tail | overlay              |
| `PrintAndExit`      | stdout plain/rich printer | none                    | none                 |
| `Dump`              | deterministic buffer      | deterministic buffer    | deterministic buffer |


The current bug exists because `LiveInline` effectively has two transcript owners.

### Entire History Requirement

To display entire history robustly:

1. Keep committed transcript cells in memory/state.
2. Insert finalized cells into terminal scrollback as they finalize.
3. On selecting a completed task, clear owned scrollback and replay all retained cells once.
4. Keep the live viewport small and control-focused.
5. Use full-screen transcript overlay for interactive browsing/search/copy.
6. On resize, rebuild scrollback from source cells.
7. Cap replay only by an explicit setting, never accidentally via viewport height.

This satisfies "entire history" without re-rendering it twice.

### Performance Plan

Use caches with explicit invalidation:

- projection cache: events -> transcript nodes;
- cell cache: nodes -> render cells;
- line cache: `(cell_id, width, mode, revision) -> Vec<Line>`;
- height cache: `(cell_id, width, mode, revision) -> u16`;
- scrollback replay cache: retained suffix for current width.

Frame-time work should be:

- render active cell;
- render dock;
- flush newly finalized history lines;
- draw diff.

Frame-time work should not be:

- rebuild all chronological events;
- re-render all completed transcript lines;
- re-interpret child sessions;
- sort all events repeatedly.

### Wrapping Plan

Adopt Codex-like wrapping rules:

- use `unicode-width` for visible cell width;
- use `textwrap` for word wrapping;
- use Ratatui `Paragraph::line_count` for height when rendering paragraphs;
- preserve URL tokens for clickable terminal links;
- centralize prefixing/indentation helpers;
- add raw output mode for clean selection/copy;
- treat ANSI/OSC escape sequences as zero-width when measuring.

### Styling Plan

Create a small style system:

- default foreground for primary text;
- dim for secondary;
- one accent for selection/status;
- green for success;
- red for errors;
- avoid arbitrary RGB;
- avoid hard white/black foreground;
- keep status and transcript visually distinct.

Most of our jank is structural, but consistent styling will make regressions easier to see.

### Testing Plan

We need four test layers.

#### Projection Tests

Golden fixtures:

```text
events -> transcript nodes
```

Assertions:

- child session output is not top-level parent answer;
- parent subagent summary appears once;
- tool groups are stable;
- final answer appears once.

#### Cell Snapshot Tests

For each cell type:

- narrow width;
- normal width;
- long URL;
- long command;
- multiline output;
- Unicode/wide chars;
- raw mode.

#### Terminal Driver Tests

Use a vt100-like backend or tmux smoke:

- insert history above viewport;
- replay completed session;
- resize and reflow;
- clear owned scrollback;
- no leaked ANSI;
- no duplicate transcript after replay;
- cursor restored after insertion.

#### Real TUI Smoke

Keep the repo standard:

```bash
scripts/verify-terminal-ui.sh
```

Add focused scenarios:

- load latest completed task;
- select task from history;
- press `Tab`, open/close history;
- open full transcript overlay;
- resize terminal;
- paste multi-line text;
- run subagent task;
- completed parent answer with child output;
- browser overlay open/close.

### Migration Sequence

Do this in small steps.

1. Introduce `TranscriptNode` projection with tests.
2. Implement `RenderCell` for existing visible event types.
3. Make existing Ratatui body render from cells, still without native scrollback changes.
4. Replace child-event flattening with explicit child summary nodes.
5. Move history picker to full-screen overlay.
6. Introduce `TerminalDriver` and pending scrollback insertion.
7. In `LiveInline`, stop drawing committed transcript in the Ratatui body.
8. Add resize reflow from committed cells.
9. Delete old native replay functions.
10. Delete duplicate event-to-line renderers.

Step 7 is the key behavioral switch. Steps 1-6 make it safe.

### Non-Negotiable Invariants

- One owner per visual region.
- One semantic interpretation path from event to transcript.
- Committed transcript is source-backed, not scraped from terminal output.
- Native scrollback insertion and Ratatui viewport drawing are coordinated by one terminal driver.
- Full history browsing is an overlay, not a bottom pane.
- Resize rebuilds scrollback from source cells.
- Dumps are not enough for terminal correctness.

### The Short Version

The current renderer is janky because it is half retained UI and half terminal scrollback, without an ownership boundary between them. Codex solves this by making terminal scrollback the committed transcript, the Ratatui viewport the live interactive dock, and the full transcript a separate overlay backed by typed history cells.

Our revamp should copy that ownership model, not just isolated code. The biggest wins will come from:

- typed transcript cells;
- explicit renderer modes;
- a real terminal driver;
- full-screen overlays;
- source-backed resize reflow;
- no duplicated raw-event renderers.

## Visual Appendix: Simple Mental Model

### Our Current Renderer

Our current renderer has one stream of persisted events, but several places turn those events into visible transcript text.

```text
                      SQLite sessions + events
                               |
                               v
                      AppStateCache / projection
                               |
            +------------------+------------------+
            |                                     |
            v                                     v
   Ratatui main renderer                  native transcript replay
   render_main()                          maybe_emit_native_transcript()
            |                                     |
            v                                     v
   draws transcript/body/composer          inserts transcript into
   inside live terminal viewport           terminal scrollback
            |                                     |
            +------------------+------------------+
                               |
                               v
                     real terminal screen
```

The problem is that both branches can show the same committed transcript.

```text
Terminal after selecting a completed task

┌──────────────────────────────────────────────┐
│ native scrollback copy of transcript          │  <- inserted by native replay
│ native scrollback copy of transcript          │
│ native scrollback copy of transcript          │
├──────────────────────────────────────────────┤
│ Ratatui live viewport                         │
│ same transcript rendered again                │  <- drawn by render_main
│ history picker / composer / footer            │
└──────────────────────────────────────────────┘
```

There is a second duplication path for subagents:

```text
parent session events
   |
   +-- parent agent.completed says: "subagent result is X"
   |
   +-- child session events are also included
          |
          +-- child session.done says: "result is X"

visible output:
   X appears as child answer
   X appears again as parent subagent result
```

So the root issue is not just rendering bugs. The renderer lacks a rule for who owns each piece of visible content.

### Codex Renderer

Codex makes the ownership split explicit.

```text
                  protocol/app events
                          |
                          v
                  typed HistoryCell objects
                          |
          +---------------+----------------+
          |                                |
          v                                v
 committed transcript cells          active in-flight cell
 App.transcript_cells                ChatWidget.active_cell
          |                                |
          v                                v
 terminal scrollback                 live Ratatui viewport
 insert_history.rs                   ChatWidget + BottomPane
```

Normal Codex chat mode looks like this:

```text
Real terminal

┌──────────────────────────────────────────────┐
│ committed history in real scrollback          │
│ user prompt                                   │
│ assistant answer                              │
│ tool result                                   │
│ next user prompt                              │
│ ...                                           │
├──────────────────────────────────────────────┤
│ inline Ratatui viewport                       │
│ active streaming answer/tool call             │
│ status row                                    │
│ composer                                      │
│ footer hints                                  │
└──────────────────────────────────────────────┘
```

The committed transcript is not redrawn in the live viewport. It already lives in terminal scrollback.

When Codex opens the full transcript, it switches ownership:

```text
Ctrl+T transcript overlay

┌──────────────────────────────────────────────┐
│ alternate screen pager owns whole terminal    │
│                                              │
│ committed transcript cells                    │
│ committed transcript cells                    │
│ optional live tail from active cell           │
│                                              │
│ scroll/page/jump/close hints                  │
└──────────────────────────────────────────────┘
```

That is why Codex can show history in two ways without duplicating it:

- normal mode: terminal scrollback owns committed history;
- overlay mode: full-screen pager owns committed history;
- those modes are not active as competing transcript owners at the same time.

### The Difference In One Picture

Our current model:

```text
                 same transcript
                       |
        +--------------+--------------+
        |                             |
        v                             v
 terminal scrollback             Ratatui body
        |                             |
        +--------------+--------------+
                       |
                       v
                  duplicated UI
```

Codex model:

```text
                 same transcript
                       |
                 renderer mode?
                       |
        +--------------+--------------+
        |                             |
        v                             v
 normal chat                    transcript overlay
 scrollback owns it             pager owns it
        |                             |
        v                             v
 live viewport only             whole screen pager
 shows active/dock              shows history
```

### The Rule We Should Adopt

Every renderer mode should answer this table before drawing:

```text
┌────────────────────┬────────────────────┬──────────────────────┐
│ Mode               │ History owner       │ Live UI owner         │
├────────────────────┼────────────────────┼──────────────────────┤
│ Normal chat        │ terminal scrollback │ inline viewport/dock  │
│ History picker     │ full-screen overlay │ full-screen overlay   │
│ Browser/debug      │ full-screen overlay │ full-screen overlay   │
│ Print completed    │ stdout printer      │ none                 │
│ Dump test          │ test buffer         │ test buffer           │
└────────────────────┴────────────────────┴──────────────────────┘
```

If two owners try to draw the same history in the same mode, that is a bug.

### The Practical Fix In One Sentence

For the normal TUI, replay completed transcript into terminal scrollback once, make the Ratatui viewport only render active work plus controls, and move history browsing into a full-screen overlay.

## Part 6: Ideal Renderer Proposal

This section is intentionally more aggressive than the migration plan above. The goal is not to patch the current renderer into being less bad. The goal is to define the renderer we would build if we were allowed to design it cleanly, then migrate toward it.

### Should We Revamp From Scratch?

Yes, for the renderer specifically.

Not because every existing component is useless. The store, event log, projections, providers, Python worker, and agent loop can stay. But the TUI renderer should be treated as a new subsystem because the current one has the wrong boundaries.

The current renderer mixes:

- event interpretation;
- transcript formatting;
- terminal scrollback insertion;
- live viewport drawing;
- overlays;
- keyboard routing;
- history browsing;
- debug/developer surfaces;
- plain transcript export.

That makes every fix risky because a change in one visual path affects another path accidentally.

The ideal renderer should instead have one job:

> turn stable UI models into terminal pixels, with one owner for every visual region.

### Ideal Renderer Principles

1. Events are not UI.
2. Transcript is source-backed and typed.
3. Rendering is mode-based.
4. Normal chat uses terminal scrollback for committed history.
5. The live viewport renders only active work and controls.
6. Full history browsing is a full-screen overlay.
7. Resize rebuilds from source, not from terminal output.
8. Rendering has measurement and drawing phases.
9. Keyboard routing follows active surface ownership.
10. Dumps, snapshots, and real terminal smoke tests all exercise different renderer layers.

### High-Level Architecture

```text
SQLite event log
      |
      v
SessionProjector
      |
      v
TranscriptModel
      |
      v
RenderModel
      |
      v
Renderer
      |
      +--> TerminalScrollback  (committed history)
      |
      +--> LiveViewport        (active work + dock)
      |
      +--> OverlayViewport     (history/browser/debug/setup)
      |
      +--> PlainPrinter        (print-and-exit/export)
      |
      +--> DumpRenderer        (deterministic tests)
```

Each arrow is a boundary. Data gets more visual as it moves downward. It should never move backward.

### Proposed Modules

```text
crates/browser-use-tui/src/
  renderer/
    mod.rs
    mode.rs
    driver.rs
    frame.rs
    layout.rs
    style.rs
    wrapping.rs
    scrollback.rs
    viewport.rs
    overlay.rs
    plain.rs
    dump.rs

  transcript/
    mod.rs
    node.rs
    projector.rs
    cell.rs
    cache.rs
    child_sessions.rs

  surfaces/
    dock.rs
    composer.rs
    active_cell.rs
    history_overlay.rs
    browser_overlay.rs
    developer_overlay.rs
    setup_overlay.rs
```

The exact filenames can change, but the boundaries should not.

### Core Types

#### RendererMode

```rust
enum RendererMode {
    LiveInline,
    Overlay(OverlayKind),
    PrintAndExit,
    Dump(DumpKind),
}
```

This replaces implicit behavior like "if native history is active, also maybe render live replay lines".

Every draw starts by choosing a mode. The mode determines ownership.

#### VisualOwnership

```rust
struct VisualOwnership {
    history: HistoryOwner,
    active: ActiveOwner,
    input: InputOwner,
    overlay: Option<OverlayOwner>,
}
```

Example:

```text
LiveInline:
  history = TerminalScrollback
  active  = InlineViewport
  input   = Dock
  overlay = None

Overlay(History):
  history = OverlayPager
  active  = OverlayLiveTail
  input   = Overlay
  overlay = History
```

If a mode would assign two owners to the same content, it should be impossible to represent.

#### TranscriptNode

```rust
struct TranscriptNode {
    id: TranscriptNodeId,
    session_id: SessionId,
    parent_session_id: Option<SessionId>,
    source_event_ids: Vec<EventId>,
    kind: TranscriptNodeKind,
    visibility: TranscriptVisibility,
    revision: u64,
}
```

`TranscriptNodeKind`:

```rust
enum TranscriptNodeKind {
    UserPrompt(UserPromptNode),
    AssistantMessage(AssistantMessageNode),
    Reasoning(ReasoningNode),
    ToolCall(ToolCallNode),
    ToolResult(ToolResultNode),
    ToolGroup(ToolGroupNode),
    SubagentStarted(SubagentStartedNode),
    SubagentResult(SubagentResultNode),
    BrowserState(BrowserStateNode),
    Artifact(ArtifactNode),
    Error(ErrorNode),
    SystemNotice(SystemNoticeNode),
}
```

This is where subagent duplication gets solved. Child sessions become child-owned nodes. Parent rendering can include a child summary node, but it does not also flatten the child answer into parent answer text.

#### RenderCell

```rust
trait RenderCell {
    fn id(&self) -> RenderCellId;
    fn source_node_id(&self) -> TranscriptNodeId;
    fn revision(&self) -> u64;
    fn display_lines(&self, width: u16, mode: DisplayMode) -> Vec<Line<'static>>;
    fn plain_lines(&self) -> Vec<String>;
    fn desired_height(&self, width: u16, mode: DisplayMode) -> u16;
    fn is_continuation(&self) -> bool;
}
```

Cells are visual objects. They are not database events.

Examples:

- `UserPromptCell`;
- `AssistantMarkdownCell`;
- `ToolCallCell`;
- `ToolOutputCell`;
- `ToolGroupCell`;
- `SubagentSummaryCell`;
- `BrowserSnapshotCell`;
- `ArtifactCell`;
- `ErrorCell`;
- `SessionHeaderCell`.

#### Surface

```rust
trait Surface {
    fn id(&self) -> SurfaceId;
    fn desired_size(&self, constraints: Constraints) -> SizeHint;
    fn render(&self, frame: &mut Frame, area: Rect);
    fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome;
}
```

Surfaces own their own input. The root app only routes to the active surface.

### Ideal Normal Chat Layout

Normal mode should not look like a full-screen app. It should behave like a terminal-native chat program.

```text
Real terminal

┌────────────────────────────────────────────────────┐
│ real terminal scrollback                           │
│                                                    │
│  task loaded                                       │
│  user prompt                                       │
│  assistant answer                                  │
│  tool call                                         │
│  tool output                                       │
│  final answer                                      │
│                                                    │
├────────────────────────────────────────────────────┤
│ inline viewport owned by renderer                  │
│                                                    │
│  active streaming answer or active tool group       │
│  compact status/progress                            │
│  pending approvals / queued followups if any        │
│  composer                                           │
│  footer hints                                       │
└────────────────────────────────────────────────────┘
```

The live viewport is not a duplicate transcript window. It is a control dock plus currently changing content.

### Ideal Completed Task Load

When loading a completed task:

```text
select session
    |
    v
load events
    |
    v
project TranscriptModel
    |
    v
build RenderCells
    |
    v
clear renderer-owned scrollback
    |
    v
insert committed cells into terminal scrollback once
    |
    v
draw small dock viewport
```

The final screen:

```text
┌────────────────────────────────────────────────────┐
│ completed transcript in terminal scrollback         │
│ appears once                                        │
│ selectable as normal terminal text                  │
│ scrollable with terminal scrollback                 │
├────────────────────────────────────────────────────┤
│ composer / followup prompt                          │
│ footer hints                                        │
└────────────────────────────────────────────────────┘
```

There is no Ratatui body copy of the completed transcript.

### Ideal Running Task

For a running task:

```text
committed cells       -> terminal scrollback
active mutable cell   -> live viewport
composer/status       -> dock
```

As soon as active content finalizes:

```text
active cell finalizes
    |
    v
append to TranscriptModel
    |
    v
insert into terminal scrollback
    |
    v
clear/replace active viewport area
```

The active cell should not remain rendered in the viewport after it has been committed to scrollback.

### Ideal Overlay Model

Overlays are not panes. They own the full screen.

```text
Overlay mode

┌────────────────────────────────────────────────────┐
│ overlay header                                     │
├────────────────────────────────────────────────────┤
│ overlay content                                    │
│ - history picker                                   │
│ - full transcript                                  │
│ - browser state                                    │
│ - debug/developer info                             │
│ - setup/provider/auth                              │
├────────────────────────────────────────────────────┤
│ overlay hints                                      │
└────────────────────────────────────────────────────┘
```

Opening an overlay:

1. save live inline viewport;
2. enter alternate screen if enabled;
3. render overlay;
4. route all keys to overlay;
5. on close, leave alternate screen;
6. restore inline viewport and redraw dock.

### Ideal History Picker

The current `Tab` behavior should become this:

```text
Tab
 |
 v
HistoryOverlay

┌────────────────────────────────────────────────────┐
│ History                                            │
├────────────────────────────────────────────────────┤
│ > latest task                  done      11h ago    │
│   inspect repository           done      11h ago    │
│   read-only reconnaissance     done      12h ago    │
│   ...                                              │
├────────────────────────────────────────────────────┤
│ Enter open  R resume  Esc close                    │
└────────────────────────────────────────────────────┘
```

Selecting a row closes the overlay and replays that session into scrollback once.

### Ideal Full Transcript Overlay

Full transcript is different from normal scrollback:

- scrollback is terminal-native and always available;
- transcript overlay is structured, searchable, and width-aware;
- it can include folded sections, child session expansion, and metadata.

```text
Ctrl+T

┌────────────────────────────────────────────────────┐
│ Transcript                                         │
├────────────────────────────────────────────────────┤
│ user: make 2-3 tool calls...                       │
│                                                    │
│ assistant: ...                                     │
│                                                    │
│ tool: read README.md                               │
│ result: ...                                        │
│                                                    │
│ subagent: repo-explorer                            │
│   status: completed                                │
│   result: ...                                      │
├────────────────────────────────────────────────────┤
│ / search  ↑↓ scroll  Enter expand  Esc close       │
└────────────────────────────────────────────────────┘
```

Normal mode scrollback is for reading/selecting text. Transcript overlay is for structured navigation.

### Ideal Terminal Driver

The terminal driver should be responsible for every raw terminal side effect.

```rust
struct TerminalDriver {
    terminal: CustomTerminal,
    mode: RendererMode,
    viewport: ViewportState,
    scrollback: ScrollbackState,
    pending_history: Vec<HistoryBatch>,
}
```

Responsibilities:

- enable/restore terminal modes;
- manage raw mode, bracketed paste, keyboard protocol;
- track viewport area;
- draw Ratatui viewport;
- insert scrollback lines above viewport;
- clear renderer-owned scrollback;
- handle Zellij/tmux/Terminal.app quirks;
- invalidate diff buffers after raw terminal mutations;
- track cursor position/style;
- expose deterministic test backend hooks.

No other module should call raw terminal scrollback APIs.

### Ideal Scrollback Manager

```rust
struct ScrollbackManager {
    committed_cells: Vec<Arc<dyn RenderCell>>,
    rendered_width: Option<u16>,
    visible_rows: u16,
    pending_batches: VecDeque<ScrollbackBatch>,
}
```

Operations:

```rust
append_cell(cell)
replay_all(cells, width)
clear_owned_scrollback()
reflow(width, height)
set_raw_mode(enabled)
```

Rules:

- appending a committed cell queues exactly one insert;
- replay clears and reinserts;
- resize reflow clears and reinserts from source;
- live viewport never renders committed scrollback cells in normal mode.

### Ideal Render Flow

```text
App receives event
    |
    v
SessionProjector updates TranscriptModel
    |
    v
Renderer receives RendererUpdate
    |
    +-- committed node? -> build RenderCell -> queue scrollback insert
    |
    +-- active node?    -> update active viewport cell
    |
    +-- overlay open?   -> update overlay model
    |
    v
FrameRequester schedules draw
    |
    v
TerminalDriver flushes pending scrollback
    |
    v
TerminalDriver draws active viewport/overlay
```

### Ideal Caching

Cache by stable identities:

```text
TranscriptNodeId
RenderCellId
Width
DisplayMode
Revision
```

Caches:

- event projection cache;
- node-to-cell cache;
- cell display-line cache;
- cell height cache;
- overlay content-height cache;
- scrollback replay cache for current width.

Invalidation:

- event appended -> affected session nodes dirty;
- terminal width changed -> line/height cache dirty for that width only;
- active stream tick -> active cell revision changes;
- style/theme changed -> display cache dirty;
- raw mode toggled -> display mode dirty.

### Ideal Performance Target

Normal frame should be cheap:

```text
flush pending scrollback batches: O(new committed lines)
render active viewport:          O(active cell + dock)
render overlay if open:          O(visible overlay rows)
```

Normal frame should not be:

```text
O(all events)
O(all transcript lines)
O(all child sessions)
O(all markdown conversion)
```

Resize/replay can be heavier, but should be explicit, debounced, and source-backed.

### Ideal Keyboard Routing

```text
key event
   |
   v
active renderer mode
   |
   +-- Overlay open? -> overlay handles key
   |
   +-- Composer focused? -> composer handles key
   |
   +-- Running task? -> interrupt/cancel bindings
   |
   +-- Global command? -> app command
```

Each surface exposes what it handles. The root app should not know every detail of every pane.

### Ideal Subagent Rendering

Subagents need explicit hierarchy.

```text
Parent transcript

user: investigate repo

assistant:
  starting repo-explorer

subagent repo-explorer:
  status: completed
  summary: found project structure...
  [expand in transcript overlay for details]

assistant:
  final synthesis...
```

Rules:

- child internal prompts do not render as parent user prompts;
- child final answers do not render as parent assistant final answers;
- parent gets a `SubagentResultCell`;
- full transcript overlay can expand child details;
- developer/debug overlay can show raw child event stream.

### Ideal Plain Output

Print/export should use the same cells:

```text
TranscriptModel -> RenderCells -> PlainPrinter
```

No separate event renderer.

This guarantees:

- terminal UI;
- print-and-exit;
- copied/exported transcript;
- dump tests;

all agree semantically.

### Ideal Testing Standard

Renderer tests should prove ownership.

Add tests like:

```text
completed task replay:
  assert scrollback contains answer once
  assert viewport does not contain committed answer

subagent task:
  assert child result appears once in parent summary
  assert child details appear only when expanded/debug mode

history overlay:
  assert opening overlay hides normal dock body
  assert closing overlay restores dock

resize:
  assert scrollback rebuilt from cells
  assert stale wrapped rows are gone
```

The key new test type is an ownership test:

```text
given renderer mode M
given content C
assert owner(C, M) is exactly one surface
```

### Ideal Implementation Strategy

Do not rewrite everything in one massive PR. Build the new renderer beside the old one, then switch modes one by one.

Recommended sequence:

1. Add transcript node projection and golden tests.
2. Add render cells for the highest-value nodes.
3. Add new full-screen history overlay.
4. Add terminal driver abstraction, initially wrapping current terminal behavior.
5. Add scrollback manager with replay for completed sessions.
6. Add `LiveInline` mode where committed transcript is scrollback-only.
7. Port active/running task UI to active cells.
8. Add resize reflow.
9. Port print-and-exit to cells.
10. Delete old event-to-line renderers.
11. Delete old bottom-pane history surface.

This is a rewrite by architecture, not a reckless rewrite by file deletion.

### Ideal End State

```text
                 events
                   |
                   v
          source-backed transcript
                   |
                   v
             typed render cells
                   |
                   v
          explicit renderer mode
                   |
      +------------+------------+
      |            |            |
      v            v            v
 scrollback     viewport      overlay
 committed      active/dock   structured UI
 history        only          full-screen
```

The renderer becomes predictable because every mode has one content owner. That is the main thing we need.
