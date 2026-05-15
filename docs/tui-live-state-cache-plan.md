# TUI Live State Cache Plan

## Problem

The TUI currently treats SQLite as both durable storage and the live render model. That means normal repaint paths can read sessions and events from SQLite, rebuild a `WorkbenchState`, and then render. This is the wrong hot path for an interactive composer.

The target architecture is:

```text
terminal input -> in-memory App state -> redraw immediately
agent/store writes -> SQLite commit -> process-local notification -> in-memory cache catch-up -> redraw
SQLite -> startup hydration, recovery, external-process fallback
```

SQLite remains the durable source of truth. The Rust TUI process owns the live UI state in memory.

## Audit Summary

The current hot paths and bypasses are:

- `crates/browser-use-tui/src/main.rs`
  - `App::workbench_state` calls `Store::list_sessions`, `Store::events_for_session`, then `project_workbench`.
  - `render` reaches this via `render.rs`.
  - `main_selection_count`, `execute_failed_selection`, `request_open_browser`, and `should_print_and_exit` call `workbench_state`.
  - `maybe_emit_native_transcript` calls `workbench_state`, `events_for_session`, and `events_after_seq`.
  - command paths such as submit/cancel/open-browser call `load_session` or append events; these are not per-keystroke but should update the cache.
- `crates/browser-use-tui/src/render.rs`
  - `render` calls `app.workbench_state`.
  - `developer_lines` directly calls `app.store.events_for_session`, bypassing `workbench_state`.
- `crates/browser-use-core/src/lib.rs`
  - agent execution emits many `Store::append_event` calls.
  - parallel tool calls reopen stores with `Store::open(state_dir)` inside spawned threads.
  - child-agent runs and parent-update paths append events and update child edge state.
- `crates/browser-use-store/src/lib.rs`
  - there is no process-local event queue today; SQLite tables are the implicit queue.

The notification design must cover every `Store` instance opened inside the TUI process, not only the `Store` held by `App`.

## Non-Goals

- Do not replace SQLite persistence.
- Do not introduce a polling-only implementation as the main live update path.
- Do not make `browser-use-core` depend on `browser-use-tui`.
- Do not optimize projection internals until SQLite is removed from render-time paths.

## Proposed Design

### Store Notifications

Add notification types in `browser-use-store`, not in the TUI crate:

```rust
#[derive(Clone, Debug)]
pub enum StoreNotification {
    SessionsChanged,
    SessionChanged { session_id: String },
    EventsChanged { session_id: String, seq: i64 },
    SettingsChanged,
}

pub type StoreNotifier = std::sync::mpsc::Sender<StoreNotification>;
```

Add an optional notifier to `Store`:

```rust
pub struct Store {
    state_dir: PathBuf,
    conn: Connection,
    notifier: Option<StoreNotifier>,
}
```

Add constructors:

```rust
Store::open(path)
Store::open_with_notifier(path, notifier)
Store::notifier()
```

`Store::open` remains unchanged for CLI/tests. `open_with_notifier` is used by the TUI and by stores opened from TUI-owned agent/tool threads.

Notification rules:

- Send only after the SQLite transaction commits.
- Notification send failures are ignored; they only mean the UI receiver is gone.
- Coalesce in the UI, not in the store.
- `append_event_with_identity` sends `EventsChanged { session_id, seq }`.
- If `append_event_with_identity` changed session status or updated timestamp, also send `SessionChanged { session_id }`.
- `create_session` / `create_child_session` send `SessionsChanged` or `SessionChanged`.
- `set_status`, `set_child_agent_status`, `close_child_agent`, `send_agent_message`, `set_setting`, and `delete_setting` send the narrowest useful notification.

### Notifier Propagation

The TUI creates one channel:

```rust
let (store_tx, store_rx) = std::sync::mpsc::channel::<StoreNotification>();
let store = Store::open_with_notifier(&args.state_dir, store_tx.clone())?;
```

Pass the notifier into every store opened inside the TUI process:

- `App::start_agent_for_session` passes a clone into `run_agent_thread`.
- `run_agent_thread` opens `Store::open_with_notifier`.
- `browser-use-core` keeps the notifier on `Store`.
- When core opens new stores inside parallel tool threads, use:

```rust
let notifier = store.notifier();
thread::spawn(move || {
    let store = Store::open_with_optional_notifier(state_dir, notifier)?;
    ...
})
```

This avoids a TUI dependency in core while preserving live notifications from nested store instances.

### App State Cache

Add an in-memory cache owned by `App`:

```rust
struct AppStateCache {
    sessions: Vec<SessionMeta>,
    events_by_session: HashMap<String, Vec<EventRecord>>,
    last_seq_by_session: HashMap<String, i64>,
    projected: WorkbenchState,
    projection_key: ProjectionKey,
    dirty_projection: bool,
}

struct ProjectionKey {
    selected_session_id: Option<String>,
    browser: String,
    history_tasks_visible: bool,
}
```

Startup hydration:

1. Load settings as today.
2. Load sessions once.
3. Load events for all sessions once. This is acceptable for the current scale and makes history navigation instant. Optimize later if necessary.
4. Build initial `WorkbenchState`.

Cache update methods:

- `hydrate_from_store(&Store)`
- `apply_notification(&Store, StoreNotification)`
- `refresh_session(&Store, session_id)`
- `refresh_sessions(&Store)`
- `refresh_events_after_seq(&Store, session_id)`
- `cached_workbench_state(&mut self) -> &WorkbenchState`

On `EventsChanged`, use `events_after_seq(session_id, last_seq)` and append to memory.

On `SessionChanged`, reload just that session if possible. If ordering changes are hard to maintain correctly, call `list_sessions` on notification only, not per render.

On `SessionsChanged`, call `list_sessions` and hydrate events for any unknown sessions.

### Render Contract

After this change:

- `render.rs` must not call SQLite.
- `render` uses `app.cached_workbench_state()`.
- `developer_lines` uses cached events, not `app.store.events_for_session`.
- native scrollback uses cached state and cached event slices. It can still write terminal scrollback, but it cannot query SQLite while drawing.
- command handlers can write SQLite, but they must also update or receive notifications into the cache before the next draw.

### Event Loop Contract

The event loop should process three inputs:

- terminal events from crossterm
- store notifications from `store_rx.try_recv`
- fallback ticks

Rules:

- Keystrokes mutate `app.composer` and set `draw_needed = true`.
- Keystrokes do not read SQLite.
- A keypress should be followed by a redraw before draining a large key backlog.
- Paste remains efficient because bracketed paste arrives as one paste event.
- Store notifications update the cache and set `draw_needed = true`.
- Fallback polling runs every 500-1000 ms and catches external processes or missed notifications.

### Fallback Polling

Fallback polling is still needed because other processes may mutate the same SQLite state:

- CLI commands
- imported sessions
- old tests or scripts
- a crashed/restarted agent process

Fallback behavior:

- Check session list revision cheaply, or just call `list_sessions` at low frequency.
- For known sessions, call `events_after_seq` using the in-memory `last_seq_by_session`.
- Never run fallback polling from a keystroke handler.

## Implementation Phases

### Phase 1: Instrument and Guard the Hot Path

- Add temporary timing or debug counters around:
  - `draw_terminal_frame`
  - `App::workbench_state`
  - `Store::list_sessions`
  - `Store::events_for_session`
  - `project_workbench`
- Add a test seam or debug assertion proving render does not hit the store after the cache migration.

### Phase 2: Add Store Notifications

- Add `StoreNotification` and optional notifier to `browser-use-store`.
- Notify after commits in store mutation methods.
- Add unit tests:
  - append event sends `EventsChanged`
  - status-changing event also sends `SessionChanged`
  - session creation sends session notification
  - settings write sends settings notification

### Phase 3: Propagate Notifiers Through TUI-Owned Stores

- Open the TUI store with a notifier.
- Pass notifier into `run_agent_thread`.
- Open the runtime store with the notifier.
- Preserve notifier when core opens parallel tool stores.
- Add tests or assertions around nested store notification propagation where practical.

### Phase 4: Add `AppStateCache`

- Hydrate sessions/events once at startup.
- Replace `App::workbench_state` with cached projection.
- Add incremental update methods using `events_after_seq`.
- Keep old direct store reads temporarily only behind clearly named transitional methods.

### Phase 5: Remove Render-Time SQLite Reads

- Change `render.rs` to consume cached `WorkbenchState`.
- Change `developer_lines` to consume cached events.
- Change native scrollback functions to use cached state/events.
- Audit with `rg` to confirm render paths do not call:
  - `list_sessions`
  - `events_for_session`
  - `events_after_seq`
  - `load_session`
  - `get_setting`

### Phase 6: Event Loop Update

- Drain store notifications each loop iteration.
- Apply notifications before drawing.
- Draw immediately after a terminal key event.
- Avoid draining a large key backlog before the first redraw.
- Keep bracketed paste as a single event.

### Phase 7: Tests and Verification

Add or update tests:

- cache hydrates startup state
- cache applies `EventsChanged`
- cache applies `SessionChanged`
- render/developer/native-scrollback paths use cache only
- keystroke path does not query SQLite
- store notifications fire for agent and nested parallel tool stores

Add a real tmux smoke to `scripts/tui-terminal-smoke.py`:

- launch running TUI
- type a long literal string quickly
- assert the prefix appears within a tight timeout
- assert no duplicate UI chrome
- assert no leaked escape sequences or bracketed paste markers

Final verification command:

```bash
scripts/verify-terminal-ui.sh
```

## Definition of Done

- No SQLite reads occur during normal repaint.
- No SQLite reads occur while handling ordinary composer character input.
- Agent events appear live through notifications, not live polling.
- Fallback polling only runs on a low-frequency timer.
- History, browser panel, developer panel, native scrollback, follow-up, retry, cancel, and child-agent activity still work.
- `scripts/verify-terminal-ui.sh` passes.
- `/tmp/but-design-loop/` artifacts show responsive composer input and no terminal artifacts.

## Risks

- Missed notifier propagation from stores reopened inside worker threads. Mitigation: preserve `Store::notifier()` and audit every `Store::open` in TUI-owned execution paths.
- Projection still expensive with very large histories. Mitigation: dirty projection and projection keys first; optimize protocol projection later if needed.
- Native scrollback currently has special terminal side effects. Mitigation: keep the terminal-writing logic but feed it cached events only.
- External SQLite mutations cannot notify the running TUI. Mitigation: low-frequency fallback polling remains.
