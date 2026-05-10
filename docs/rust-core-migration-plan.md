# Rust Core Migration Plan

Status: execution plan for the Rust-first rewrite starting from `main`.

This branch should not be a port of the old Textual UI or the experimental Rust TUI branch. It should take the primitive product idea from `main`, keep the good browser-harness behavior, and rebuild the runtime around one simple event-driven core.

The guiding rule:

```text
Rust owns durable product state.
Python owns volatile browser state.
Events are the contract.
```

## High-Level Overview

The repository should become a Rust-first browser-agent runtime with one deliberate Python island.

At runtime:

1. The user enters a browser task in the Rust TUI or CLI.
2. Rust creates or resumes a durable task record in SQLite.
3. Rust appends normalized events for every meaningful state transition.
4. The Rust agent loop streams model events, dispatches tools, handles cancellation, and records results.
5. The model uses one browser-connected Python tool for page work.
6. The Python worker talks to the browser through a browser-harness-shaped daemon.
7. The TUI renders a small `WorkbenchState` projection, not raw logs.

## Design Principles

- Start from the UI and product flow, not from internal session/debug concepts.
- Keep state in one SQLite boundary instead of scattered JSON, JSONL, runner, and cancel files.
- Keep Python where it is actually valuable: live browser control, raw CDP, helper code, scraping/PDF/image/table libraries.
- Do not create a generic Rust browser/CDP abstraction.
- Keep the model-visible tool surface tiny.
- Copy only the Codex sub-agent ideas that prevent context blowups.
- Treat every extra concept as suspect until it proves product value.

## Repository Shape

Target shape:

```text
crates/
  browser-use-protocol/
    EventRecord, SessionMeta, ToolCall, ToolResult, ModelEvent, WorkbenchState

  browser-use-store/
    SQLite store, migrations, event append, projections, import/export

  browser-use-core/
    Agent loop, scheduling, cancellation, resume, compaction, sub-agent control

  browser-use-providers/
    fake, OpenAI Responses, Codex Responses, Anthropic Messages, OpenRouter chat

  browser-use-python-worker/
    Rust-side supervisor and RPC client

  browser-use-cli/
    user CLI, datasets, diagnostics

  browser-use-tui/
    Ratatui workbench

python/
  llm_browser_worker/
    browser-harness-shaped daemon/client
    model-editable helper namespace
    browser skills
```

Keep dependency direction strict:

```text
protocol <- store <- core
protocol <- providers
core -> providers/store/python-worker
cli/tui -> core/store/protocol
python worker -> Rust host RPC for events/cancel/artifacts only
python worker -> Python browser harness for CDP
```

## State Model

SQLite is the source of truth:

```text
.browser-use-terminal/
  state.db
  artifacts/
    <session-id>/
```

Core tables:

- `sessions`: durable task metadata, status projection, cwd, artifact root, optional parent.
- `events`: append-only event log.
- `artifacts`: files created by tools with metadata and originating event.
- `runs`: active/completed process run metadata.
- `agent_edges`: parent-child agent graph.
- `agent_messages`: mailbox messages between parent/child agents.
- `app_settings`: tiny durable product settings such as setup completion.

Files remain useful for artifacts and JSONL export/import, but not as primary state channels.

## Event Model

Durable behavior should be expressed as normalized events:

- `session.created`
- `session.input`
- `session.followup`
- `session.cancel_requested`
- `session.cancelled`
- `session.done`
- `session.failed`
- `model.delta`
- `model.usage`
- `tool.started`
- `tool.output`
- `tool.image`
- `tool.finished`
- `tool.failed`
- `browser.page`
- `browser.live_url`
- `browser.state`
- `agent.spawned`
- `agent.message`
- `agent.closed`

Only Rust writes durable events. Python can request event emission through host RPC, but it should not write directly to SQLite.

## TUI Plan

The TUI follows `docs/terminal-ui-product-ux.md`.

Normal product screens:

- first-run setup
- ready workbench
- running task
- result
- failure
- browser overlay
- history overlay
- action menu

The normal UI renders product projections:

```text
events/tool output/browser state
  -> activity summary
  -> browser summary
  -> result summary
  -> history row
```

Default UI must not expose raw events, traces, artifacts, provider internals, compaction, or tool logs. Those can exist behind hidden developer/debug mode.

## Python Browser Worker Plan

Python remains the browser tool worker.

Python owns:

- persistent per-task Python namespace
- browser CDP websocket and active target/session identity
- reconnect and stale-session recovery
- raw `cdp(...)`
- browser helper functions
- screenshots/downloads/network/dialog/console helpers
- Python libraries for scraping, PDFs, tables, and images
- helper code the model can read and edit

Rust owns:

- starting/stopping the worker process
- cancellation
- durable event writes
- artifact indexing
- model-visible tool results
- session lifecycle

Tool flow:

```text
Rust receives python tool call
  -> Python worker executes code against live browser harness
  -> Python returns text/data/images/artifacts
  -> Rust appends tool and artifact events
  -> model receives compact result and image context
```

## Agent Core Plan

Rust owns the agent loop:

- provider turn loop
- tool-call dispatch
- cancellation checks
- deadlines
- resume from events
- compaction
- final result handling
- event emission
- sub-agent lifecycle

The provider boundary should be thin:

```rust
enum ModelEvent {
    TextDelta(String),
    ToolCall(ToolCall),
    Usage(ModelUsage),
    Done,
}
```

Use mature Rust crates where they fit. Current package checks for this branch found:

- `ratatui 0.30.0` for TUI rendering
- `rusqlite 0.39.0` for SQLite
- `crossterm 0.29.0` for terminal IO
- `async-openai 0.34.0` worth evaluating for OpenAI-compatible providers
- `reqwest-eventsource 0.6.0` worth evaluating for SSE if provider streaming needs a helper crate
- Anthropic Rust SDKs exist, but the ecosystem appears weaker; prefer a thin `reqwest` adapter unless a crate cleanly supports streaming, tools, vision, and auth needs

## Sub-Agent Plan

Keep only the Codex parts that solve context growth.

Required concepts:

- child agents are real sessions
- parent-child graph edges are durable
- task paths are canonical and human-readable, so helpers can be addressed by `/root/<task>` instead of only ephemeral session ids
- child history is isolated
- forked history is sanitized
- a mailbox lets parent/child communicate without copying full transcripts
- `wait_agent` waits for child messages/final status
- child final answers summarize back to the parent

Default model-visible tools:

```text
spawn_agent
wait_agent
send_message
followup_task
list_agents
close_agent
```

Do not copy Codex file editing, shell, patch, app-server, remote-thread, Guardian, feedback, or legacy multi-agent product surfaces.

## Migration Phases

### Phase 1: Product Workbench And SQLite Foundation

Implemented foundation in this branch:

- Rust workspace
- `browser-use-protocol`
- `browser-use-store`
- `browser-use-tui`
- `browser-use-cli`
- SQLite schema for sessions/events/artifacts/runs/agent_edges
- event-derived `WorkbenchState`
- transactional event append plus status projection updates
- SQLite event subscription primitives: read/wait for events after a sequence number
- legacy session export/import for `session.json` + `events.jsonl`
- deterministic setup/ready/result/cancel/developer-overlay TUI tests
- deterministic `--overlay` dump hook for setup-flow and workbench overlays
- manual CLI-to-TUI shared-store smoke
- Rust-supervised Python worker with persistent per-task namespace
- CLI `python` command records `tool.started`, `tool.output`, `tool.finished`, and `tool.failed`
- local browser-harness helpers load into the Python worker when available
- `browser-use-core` has a provider-driven tool loop for `python` and `done`
- `browser-use-core` exposes minimal model-visible helper tools: `spawn_agent`, `wait_agent`, `send_message`, `followup_task`, `list_agents`, and `close_agent`
- fake agent path exercises model tool calls instead of bypassing the tool protocol
- `browser-use-protocol` defines typed `ToolSpec`, `ToolCall`, `ToolResult`, `ToolImage`, `ModelEvent`, and `ModelUsage`
- `browser-use-providers` has a fake provider implementing the provider turn boundary
- `browser-use-providers` has a thin OpenAI Responses adapter, covered by a local mocked HTTP test
- `browser-use-providers` has a thin Codex Responses adapter, covered by auth-file and local mocked SSE tests
- `browser-use-providers` has thin Anthropic Messages and OpenAI-compatible chat adapters, covered by local mocked HTTP tests
- OpenAI Responses input conversion preserves image parts for browser screenshots
- Rust CLI has `run-openai`, `run-codex`, `run-anthropic`, and `run-openrouter`, which use the same provider-driven agent/tool/event path as fake runs
- Rust core can run an existing SQLite session from `agent.context`, `session.input`, and follow-up events
- Rust CLI has provider-specific `run-*-session` commands for executing/resuming a spawned child session without copying the parent transcript
- Python package entry points now hand off to Rust CLI/TUI binaries through a tiny worker-island launcher
- Python worker host helpers support output chunks, copied artifacts, image records, browser live URL/state, and cancellation checks
- Python worker host helpers now emit line-framed streaming events before the final compact response
- Python image outputs are forwarded into the next model turn as data-URL `input_image` parts while still being indexed as artifacts/events
- Browser-harness `cdp` calls are lazily wrapped to ensure the Python daemon is alive before browser work
- Python worker emits current-tab browser state automatically after browser-harness-backed code runs, without Rust owning CDP ids
- artifact rows are indexed in SQLite and connected to artifact events
- durable sub-agent graph foundation with child sessions, canonical `/root/...` agent paths, graph edges, mailbox messages, list, wait, and close
- provider-driven helper tools create child sessions with configurable sanitized fork modes, support path-addressed mailbox messages/follow-up turns, and report compact status/results back to the model
- child completion/failure/cancellation updates the durable graph and emits one compact parent event
- child sessions receive a sanitized `agent.context` summary instead of a copied parent transcript
- closing a child recursively closes descendants so orphan helper sessions do not keep running conceptually
- normal TUI activity projection renders helper spawn/completion summaries without exposing child transcripts
- Rust CLI has a fake dataset runner that reads `datasets/*.json`, creates sessions, and records dataset case metadata
- Rust CLI has OpenAI, Codex, Anthropic, and OpenRouter dataset runners using the same real-provider loop and the same dataset metadata events
- Rust TUI can start the background provider loop directly from the composer, with account/model selections persisted as settings and fake/OpenAI/Codex/Anthropic/OpenRouter backends selected by a hidden development flag
- first-run setup, account/model/browser overlays, setup-complete confirmation, result follow-up, running, stopped, browser, history, actions, help, and developer surfaces have deterministic dump coverage and manual PTY coverage
- browser-harness smoke verifies navigation, page inspection, screenshot capture, image artifact indexing, Python worker helper context, and browser state emission through the Rust/Python boundary
- setup completion, account choice, model choice, provider model, browser choice, and agent backend are persisted in SQLite, not a separate config file
- Rust CLI has `config init/show/set`, `auth status`, `diagnostics`, and trace bundle export
- Rust CLI has explicit stored credential flows: `auth login`, `auth import-codex`, and `auth logout`; provider runners use stored credentials before environment/file fallbacks
- Rust core records run lifecycle rows, emits `session.status`, `model.config`, and `session.deadline_warning`, defaults provider runs to an 80-turn budget, compacts oversized contexts, and spills huge Python output to artifact-backed tool output
- Responses input compaction is protocol-safe for Codex/OpenAI-style providers: compacted system context is carried as user context, and stale historical function-call outputs are not replayed after compaction
- managed headless mode is owned by the Python browser island and prefers Playwright's bundled testing browser before system browsers, so automated tests do not attach to the user's personal Chrome profile
- Browser Use cloud mode is owned by the Python browser island when selected and `BROWSER_USE_API_KEY` is available
- live testing-browser smoke covers browser download artifact indexing and stale-session recovery without Rust owning CDP ids
- live Codex count-1 dataset smoke on `real_v14_short` passes end to end through Rust provider/core/store, Python browser worker, browser-harness helpers, FERC search/download, document extraction, and `session.done`
- stale Python `src/llm_browser` product runtime and tests are removed from the branch; the Python package surface is only the browser worker island

Next:

- keep expanding focused reconnect/stale-session regressions at the Python/browser-harness boundary as new cases appear
- implement true Claude Code OAuth/import if that account path remains a product requirement

### Phase 2: Python Browser Worker

- extract browser worker package under `python/llm_browser_worker` (implemented)
- load browser-harness helpers from the local harness source when available (implemented)
- define host RPC for output, artifact, image, browser state, and cancellation (implemented)
- keep raw CDP and helper editing first-class (implemented through browser harness)
- add reconnect/stale-session tests at the Python boundary

### Phase 3: Tiny Tool Surface

- implement `done` (implemented)
- implement `python` worker client (implemented)
- implement basic agent delegation tools over SQLite graph (implemented)
- keep shell/file/edit tools out of the default model prompt

### Phase 4: Rust Agent Loop

- fake provider first (implemented with real tool-call dispatch)
- OpenAI Responses provider second (thin blocking adapter implemented)
- Codex subscription/provider path third (thin blocking SSE adapter implemented)
- Anthropic Messages and OpenAI-compatible chat adapters (thin blocking adapters implemented)
- minimal tool scheduling
- event-owned tool output
- cancellation and result handling
- resume from event history
- run lifecycle rows
- thresholded context compaction
- huge tool-output spillover to artifacts

### Phase 5: Codex-Shaped Sub-Agents

- add canonical agent paths (implemented)
- add graph/migrations (implemented)
- add mailbox (implemented)
- add sanitized forked history and small fork-mode controls (implemented)
- add spawn/wait/list/close tools (implemented in the provider tool loop with session-id and canonical-path addressing)

### Phase 6: Providers And Datasets

- add Anthropic/OpenRouter after the loop is stable (implemented)
- run fake smoke, then short real dataset, then full regression
- fake dataset runner is implemented
- OpenAI/Codex/Anthropic/OpenRouter dataset runners are implemented
- Codex count-1 `real_v14_short` smoke is live-verified; Anthropic/OpenRouter live dataset smokes still require credentials

### Phase 7: Remove Python Core Runtime

- keep Python browser worker and helpers (implemented)
- remove Python CLI/state/provider/agent loop paths (implemented from package/repo surface)
- make `but` resolve to Rust binaries (implemented)

## Complexity To Cut

Delete or avoid:

- file-backed state as source of truth
- multiple browser runtime facades
- Rust-owned browser/CDP abstraction
- TUI-owned business logic
- normal-user artifact/trace/event screens
- broad model-visible coding tools
- provider registry magic beyond account/model/auth
- complex config precedence
- large session tool surface
- over-instrumented lifecycle events

Keep:

- `Session`
- `Event`
- `Artifact`
- `Tool`
- `Provider`
- `PythonBrowserHarness`
- `AgentLoop`
- `Store`
- `AgentGraph`
- `AgentMailbox`

## Acceptance Criteria

- TUI matches the UX doc in normal states.
- SQLite is the only durable state writer.
- Python browser worker can navigate, inspect, screenshot, download, and recover stale sessions.
- Rust fake provider can run a task end to end through the Python tool.
- Codex provider can run the same task.
- Parent agent context does not grow with child exploration.
- Dataset smoke runs through Rust.
- Python remains only for browser/tool execution.

Current verification status:

- done: TUI normal states, 80x24 PTY smoke, SQLite-backed state, fake provider path, Codex live no-browser smoke, Codex count-1 `real_v14_short` dataset smoke, sub-agent context isolation/path addressing, fake dataset smoke, Python runtime removal, browser-harness navigation/inspection/screenshot smoke, worker-boundary download artifact and refreshed browser identity tests
- done: real testing-browser download artifact indexing and forced stale-session recovery preserving target id are covered by `scripts/live-browser-boundary-smoke.sh`
- partially productized: API-key/Codex auth login/import/logout exists, but true Claude Code OAuth/import is not implemented
- not verified live: Browser Use cloud provisioning without a local `BROWSER_USE_API_KEY`, real Anthropic/OpenRouter smoke with live credentials, full dataset regression
