# Renderer V2 Complete Build Plan

This is the plan for building the real renderer, not more tactical patches on the current renderer.

The current codebase has useful fixes, but it is still renderer V1. V1 mixes native terminal scrollback, Ratatui body rendering, event projection, composer layout, and running-task status in the same control flow. That is why running tasks still look unstable in tmux and why bugs move around when one symptom is patched.

The target is a Codex-style renderer:

```text
events/runtime state
  -> TranscriptModel
  -> RenderCells
  -> TerminalDriver

Terminal scrollback: committed transcript cells
Inline viewport:    active cell + composer/status dock
Overlay/pager:      structured transcript/history views
```

## What Exists Today

Already built:

- native scrollback insertion using `maybe_emit_native_transcript`;
- native replay bookkeeping using `NativeHistoryState`;
- completed-session transcript replay into terminal scrollback;
- tactical fixes for duplicate output, stable slash palette height, composer newline flicker, padding, and multiline OSC-8 links;
- tmux smoke coverage for several V1 regressions;
- deep renderer analysis in `docs/tui-renderer-deep-dive-and-revamp-plan.md`.

These are useful, but they are not renderer V2.

## What Is Missing

Core missing pieces:

- `TranscriptModel`: one in-memory transcript model for the selected session.
- `TranscriptNode`: typed event/runtime nodes before visual rendering.
- `RenderCell`: width-aware renderable transcript units.
- `active_cell`: the currently streaming/running unit.
- `committed_cells`: finalized transcript units.
- `TerminalDriver`: the only owner of terminal lifecycle, viewport, scrollback insertion, clearing, resize, and synchronized drawing.
- `ScrollbackManager`: the only owner of committed transcript replay/insertion.
- source-backed resize reflow.
- explicit renderer modes.
- full-screen history/transcript inspector.
- one event-to-transcript reducer.
- removal of old duplicate renderers.

Still-present V1 paths that must eventually die:

- `native_replay_live_lines`;
- `tool_aware_chronological_lines`;
- `append_tool_aware_event`;
- `append_native_timeline_event`;
- `transcript_lines` as a competing visual renderer;
- content-height-triggered native scrollback mode;
- bottom-pane history surface;
- any path that draws committed transcript in the live Ratatui viewport.

## Non-Negotiable Invariants

1. Normal mode has exactly one owner for committed transcript: terminal scrollback.
2. Normal mode has exactly one owner for live changing content: inline viewport `active_cell`.
3. The live viewport never redraws committed transcript cells.
4. A finalized active cell is inserted into scrollback once, then removed from the live viewport.
5. SQLite is not live renderer truth. It is persistence, hydration, crash recovery, and audit storage.
6. Runtime events are interpreted once into transcript nodes.
7. Child/subagent events never leak as top-level parent transcript output by accident.
8. Opening slash/history/model/browser views must not recreate, purge, or resize terminal scrollback.
9. Resizing rebuilds scrollback from source cells, not from terminal text.
10. Deterministic dumps are necessary but not sufficient; tmux smoke tests are required.
11. Streaming must be visually stable: normal event updates must not make already-visible content jump.
12. The composer/status dock has a stable allocation. Ordinary typing, slash palette, status changes, and streaming updates must not recreate the terminal viewport.

## Visual Stability Contract

The renderer should feel append-only in normal chat mode.

That does not mean every internal object is immutable. It means the user-visible terminal should obey these rules:

- committed transcript is append-only in native scrollback;
- once a committed line has been inserted, it is not redrawn, moved, or replaced except during explicit session replay or resize reflow;
- live running output appears in the active viewport as an append-mostly tail;
- new tool/status/assistant lines append below previous active lines whenever possible;
- ephemeral state, such as spinners or "waiting" labels, may update in place only inside a small reserved status row;
- ephemeral state must not cause old transcript rows to move;
- composer growth must be handled inside the dock allocation, not by resizing the terminal viewport;
- slash/history/model/browser UI must be overlays or reserved dock content, not layout changes that shift transcript rows;
- when active content finalizes, it is committed to scrollback once and removed from the active viewport in one synchronized draw.

The desired illusion is:

```text
old committed transcript      stays in terminal scrollback
new committed transcript      appends to terminal scrollback
current running thing         appends inside active viewport
composer/status               stays anchored
```

The renderer may collapse, summarize, or replace live ephemeral rows, but only within the active cell. It must not rebuild the whole visible screen for normal stream deltas.

## Build Sequence

### Phase 0: Freeze V1 Patch Scope

Goal: stop spending time making V1 look like V2.

Allowed V1 changes:

- crash fixes;
- data loss fixes;
- tiny blockers that prevent V2 work;
- tests that preserve current known behavior until replacement.

Not allowed:

- new V1 transcript rendering paths;
- new event-to-line renderers;
- new mode heuristics based on content height;
- new bottom-pane history behavior.

Done when:

- V1 is treated as legacy compatibility code;
- new renderer work happens in new modules.

### Phase 1: Transcript Model

Goal: create the in-memory source of truth for what the renderer displays.

Build:

```rust
struct TranscriptModel {
    session_id: String,
    committed: Vec<TranscriptNode>,
    active: Option<TranscriptNode>,
    children: ChildSessionIndex,
    revision: u64,
}
```

Also build:

- node ids stable across replay;
- parent/child ownership metadata;
- active-vs-committed lifecycle;
- model hydration from current session/events;
- update path from live runtime/store notifications.

Done when:

- selected session can be represented without rendering text;
- active running work can be represented separately from committed history;
- child sessions are attached to parent nodes instead of flattened.

### Phase 2: Event Reducer

Goal: convert runtime/store events into transcript nodes exactly once.

Build reducers for:

- session input/follow-up;
- assistant stream deltas;
- assistant final answer;
- reasoning summaries;
- tool call start/output/end;
- command start/output/end;
- file read/list/search;
- patch changes;
- browser state/actions;
- subagent start/progress/completion/failure;
- task failure/cancellation;
- artifacts/screenshots/images.

Important rule:

```text
event -> node update
node -> render cell
```

Never:

```text
event -> visible lines in multiple places
```

Done when:

- root task fixtures produce expected nodes;
- subagent fixtures do not leak child answer as parent answer;
- failed/cancelled/running fixtures have correct active/committed state.

### Phase 3: Render Cells

Goal: give every transcript node a visual contract.

Build:

```rust
trait RenderCell {
    fn id(&self) -> RenderCellId;
    fn revision(&self) -> u64;
    fn display_lines(&self, width: u16, mode: DisplayMode) -> Vec<Line<'static>>;
    fn plain_lines(&self) -> Vec<String>;
    fn desired_height(&self, width: u16, mode: DisplayMode) -> u16;
}
```

Initial cells:

- user prompt;
- assistant markdown;
- streaming assistant markdown;
- reasoning summary/status;
- tool call group;
- command group;
- file operation group;
- browser operation group;
- subagent summary;
- error/cancelled;
- artifact/image/screenshot;
- session header.

Also build:

- `unicode-width` based wrapping;
- long URL wrapping and OSC-8 metadata support;
- markdown rendering that preserves style across wraps;
- height cache keyed by `(cell_id, revision, width, mode)`.

Done when:

- all important node types render through cells;
- no new visible output is built directly from raw events;
- narrow/wide snapshots match expected layout.

### Phase 4: Active Cell First

Goal: prove running task rendering before completed replay.

Build:

- active assistant streaming cell;
- active tool/command/subagent cells;
- mutation APIs for active cells;
- append-only visual semantics for active cells;
- a reserved ephemeral status row inside active cells for spinners/waiting labels;
- tail-follow behavior inside the active cell when active output exceeds the viewport;
- finalization path:

```text
active cell updates in viewport
active cell completes
active cell becomes committed cell
committed cell queues scrollback insertion
active viewport clears or advances
```

Done when:

- running task shows live progress without rendering committed history in viewport;
- tool output updates in place while active;
- streaming deltas append without moving previously visible committed transcript;
- spinner/status updates do not shift active output;
- completed tool output moves into scrollback once;
- final assistant answer streams in viewport and commits once;
- no blink/purge during normal streaming.

### Phase 5: Terminal Driver

Goal: one module owns terminal mechanics.

Build:

```rust
struct TerminalDriver {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    viewport: ViewportState,
    pending_scrollback: Vec<ScrollbackBatch>,
    last_size: Size,
}
```

Responsibilities:

- raw mode setup/restore;
- bracketed paste setup/restore;
- keyboard enhancement setup/restore;
- inline viewport area;
- stable dock allocation;
- synchronized updates;
- scrollback insertion above viewport;
- viewport redraw;
- no normal-keypress `Clear/Purge`;
- no viewport resize for ordinary stream/composer/palette/status updates;
- resize debounce;
- reset only for true session switch/resize/reflow.

Done when:

- no other module calls terminal scrollback APIs;
- draw loop can say "flush scrollback, draw viewport";
- slash/model/history/browser surfaces do not recreate terminal.

### Phase 6: Scrollback Manager

Goal: terminal scrollback is the committed transcript owner.

Build:

```rust
struct ScrollbackManager {
    committed_cells: Vec<Arc<dyn RenderCell>>,
    inserted_until: usize,
    width: u16,
}
```

Responsibilities:

- append committed cells;
- queue display lines for insertion;
- clear owned scrollback;
- replay all committed cells;
- track inserted width/revision;
- protect against duplicate insertion.

Done when:

- committed cell queues exactly one native insertion;
- completed task replay inserts transcript once;
- live viewport never draws committed cells in normal mode.

### Phase 7: Renderer Modes

Goal: make ownership explicit.

Build:

```rust
enum RendererMode {
    Ready,
    LiveInline,
    Overlay(OverlayKind),
    Setup,
}
```

Ownership table:

```text
Mode          committed transcript     live content          controls
Ready         none                     ready body            composer
LiveInline    terminal scrollback      active cell           dock/composer
Overlay       overlay/pager            optional live tail    overlay chrome
Setup         setup surface            setup surface         setup controls
```

Done when:

- there is no implicit "native active because content is tall" mode;
- each mode declares who owns transcript, active content, and controls.

### Phase 8: Completed Session Replay

Goal: selecting old/current completed sessions uses the same cells.

Flow:

```text
select session
hydrate TranscriptModel
build committed RenderCells
TerminalDriver clears owned scrollback
ScrollbackManager replays committed cells once
viewport draws only composer/dock
```

Done when:

- completed transcript appears once;
- terminal scrollback selection works;
- composer appears in stable dock;
- no empty-gap drift on session switching;
- no duplicate output after switching between sessions repeatedly.

### Phase 9: Running Session Integration

Goal: make the real live agent path use V2 by default.

Flow:

```text
submit task
create user prompt cell -> commit to scrollback
task starts -> active status/tool cell
events update active cell or commit cells
stream deltas update active assistant cell
completion commits active assistant cell
dock returns to follow-up mode
```

Done when:

- live LLM task can run end-to-end;
- current running task does not blink in tmux;
- current running task visually appends or updates in place only inside the active cell;
- previously visible committed transcript does not jump during streaming;
- subagent/tool events appear under correct ownership;
- completed result appears once;
- follow-up task continues same transcript model.

### Phase 10: Surfaces And Overlays

Goal: slash/history/model/browser/developer views do not interfere with transcript ownership.

Build:

- slash palette as dock overlay, fixed region;
- history as full-screen overlay/picker;
- model/browser/developer as full-screen or clearly owned overlays;
- optional transcript inspector/pager.

Done when:

- opening `/` does not change viewport height;
- opening Tab history does not mix with transcript;
- overlays do not mutate scrollback;
- closing overlays restores the inline viewport cleanly.

### Phase 11: Resize Reflow

Goal: resize never leaves stale wrapping or duplicate scrollback.

Build:

- keep committed cells as source;
- on resize, clear owned scrollback;
- re-render cells at new width;
- replay committed cells;
- preserve active cell;
- redraw dock.

Done when:

- resizing during running task does not duplicate active content;
- resizing completed task preserves transcript once;
- tmux resize smoke passes.

### Phase 12: Plain Output And Export

Goal: every output mode uses the same cells.

Replace:

- separate print-and-exit transcript renderer;
- event-to-plain-text fallbacks.

With:

```text
TranscriptModel -> RenderCells -> PlainPrinter
```

Done when:

- CLI print output matches normal transcript semantics;
- exports do not show child events as parent answers;
- markdown/plain output is consistent.

### Phase 13: Cutover And Delete V1

Goal: remove the architecture that causes current bugs.

Delete:

- old native replay functions;
- old tool-aware line renderer;
- old transcript fallback line renderer;
- bottom-pane history path;
- content-height native mode threshold;
- terminal purge/recreate paths for ordinary UI state.

Done when:

- one event reducer remains;
- one cell renderer remains;
- one terminal driver owns scrollback;
- V2 is default;
- V1 flag is removed.

## Test Plan

Every phase needs deterministic tests and real terminal tests.

Required fixture tests:

- short completed task;
- long completed task;
- running task with streaming answer;
- running task with multiple tools;
- subagent with child tool calls;
- failed task;
- cancelled task;
- multiline URL answer;
- narrow terminal width;
- resize while running;
- session switch loops.

Required tmux tests:

- launch app in tmux;
- submit live model task;
- verify running output appears without blank-frame flicker;
- capture multiple frames during streaming and assert stable anchor rows do not move;
- assert stream updates append or mutate only inside the active cell region;
- verify finalized output moves into scrollback once;
- send multiline composer input;
- open/close slash palette;
- open/close history overlay;
- switch sessions repeatedly;
- resize pane;
- capture full scrollback and visible pane;
- assert no duplicate transcript blocks;
- assert no leaked ANSI/bracketed paste markers;
- assert no stale app chrome.

Definition of done:

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
scripts/verify-terminal-ui.sh
```

Also inspect `/tmp/but-design-loop/`.

## Recommended Execution Order

Do not start with completed-session replay. Start with running mode, because that is where the architecture proves itself.

1. Add `TranscriptModel`, nodes, and reducers.
2. Add render cells and snapshots.
3. Add active cell and running-task viewport rendering.
4. Add `TerminalDriver`.
5. Add `ScrollbackManager`.
6. Wire running task through V2.
7. Wire completed replay through V2.
8. Add overlays.
9. Add resize reflow.
10. Replace plain output/export.
11. Delete V1.

The smallest meaningful milestone is:

```text
submit one live task
see user prompt in native scrollback
see active work in viewport
see final answer commit to scrollback once
composer remains stable
no tmux flicker
```

Until that milestone exists, the renderer is still V1 with patches.
