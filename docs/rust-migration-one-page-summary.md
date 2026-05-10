# Rust Migration One-Page Summary

This rewrite should be product-first and small.

```text
Rust owns:     TUI, CLI, durable state, events, agent loop, providers, orchestration
Python owns:   browser connection, CDP identity, browser helpers, task Python
SQLite owns:   sessions, events, runs, artifacts, agent graph, settings
Events own:    communication between runtime, UI, browser worker, and history
```

The product should feel like `docs/terminal-ui-product-ux.md`: a browser agent workbench, not a debug dashboard and not a coding tool.

## Architecture

Keep the target shape this small:

```text
TUI / CLI
  -> WorkbenchState projection
  -> SQLite event/session store
  -> Rust agent loop
  -> Rust provider adapters
  -> tiny tool layer
  -> Python browser worker
  -> browser-harness-shaped Python daemon
  -> Chrome / Browser Use cloud / CDP
```

If a new concept does not fit into this diagram, it needs a strong reason to exist.

## Product Surface

The normal TUI has one screen:

```text
Workbench
```

Temporary overlays:

```text
setup
browser
history
actions
developer/debug
```

Normal users should see:

```text
task
browser
account
model
result
history
setup
```

Normal users should not see:

```text
session
artifact
trace
provider
config
event
tool output
compact
```

Those words can exist internally, but they are not the product.

## Browser Boundary

Python should own the browser connection.

CDP target ids, attached session ids, execution contexts, DOM node ids, and runtime object ids are connection-scoped. Reconnects can invalidate them while the visible browser still looks unchanged. The code that executes browser actions must also understand those reconnects.

Rust records browser-visible facts as events. Python decides what the current browser connection means.

## Keep From Browser Harness

- one long-lived Python daemon holds the CDP websocket
- daemon owns current target/session identity
- raw `cdp(...)` remains first-class
- tab switches attach fresh sessions
- default domains are re-enabled after attach
- old-tab network events do not poison active-tab waits
- stale sessions recover inside the harness
- helpers stay simple Python the model can understand

Do not rewrite this layer in Rust.

## Keep From Codex

Only copy the pieces needed for browser-agent orchestration:

- real sub-agents as separate sessions
- parent-child graph edges
- canonical task paths, so helpers can be referenced as `/root/<task>` instead of only by generated session id
- mailbox/wait mechanism
- sanitized forked history with small fork-mode controls
- child final answers summarized back to the parent
- recursive child close/cancel
- SQLite migrations for durable schema changes

Do not copy Codex's coding-tool surface:

- no model-visible file editing by default
- no model-visible shell by default
- no app server
- no remote thread store
- no Guardian/feedback product flows
- no legacy multi-agent API

## Model-Visible Tools

Keep the default tool surface tiny:

```text
python       browser-connected Python harness
done         final result
spawn_agent  delegate bounded subtask
wait_agent   wait for delegated work
send_message queue a note for a helper
followup_task queue work that should trigger a helper turn
list_agents  inspect delegated work
close_agent  cancel delegated work
```

That is enough for a browser agent product.

## State Handling

Move from file-backed session state to SQLite:

```text
.browser-use-terminal/
  state.db
  artifacts/
```

Core tables:

```text
sessions
events
artifacts
runs
agent_edges
agent_messages
app_settings
```

The event log stays append-only. Session status, runner state, cancellation, artifacts, browser summary, result, and history rows are projections.

## Migration Order

1. Build the Rust workbench from the UX doc.
2. Move durable state to SQLite.
3. Make the Python browser worker browser-harness-shaped.
4. Port the tiny model-visible tool surface: `python`, `done`, and agent delegation.
5. Port the minimal Rust agent loop with fake + OpenAI + Codex providers.
6. Add Codex-shaped sub-agents after the basic loop works.
7. Port the dataset runner.
8. Delete Python core runtime, keeping only the Python browser worker/harness.
9. Add remaining providers only when the core product path is stable.

## Current Foundation In This Branch

- Rust workspace with protocol, store, and TUI crates.
- SQLite-backed sessions/events/runs/artifacts/agent_edges schema.
- Product workbench projection from append-only events.
- Rust CLI that writes to the same SQLite store as the TUI.
- SQLite event subscription primitives support reading/waiting after a known event sequence.
- Legacy `session.json` + `events.jsonl` export/import.
- Transactional event append plus status projection updates.
- Rust-supervised persistent Python worker boundary.
- CLI `python` command emits Rust-owned tool lifecycle events.
- Minimal Rust provider-driven loop can dispatch `python` and `done` tool calls and finish a task.
- The provider loop can also dispatch `spawn_agent`, `wait_agent`, `send_message`, `followup_task`, `list_agents`, and `close_agent` over the SQLite agent graph.
- Agent delegation tools accept either generated child session ids or canonical helper paths for `wait_agent`, `send_message`, `followup_task`, and `close_agent`.
- Fake agent path now exercises model tool-call dispatch instead of bypassing the tool protocol.
- Typed model/tool protocol plus fake/scripted/OpenAI Responses providers are in place.
- Typed Codex Responses provider is in place with auth-file loading and mocked SSE coverage.
- Typed Anthropic Messages and OpenAI-compatible chat providers are in place with mocked HTTP coverage.
- Rust CLI has `run-openai`, `run-codex`, `run-anthropic`, and `run-openrouter` for real-provider runs through the same event/tool path.
- Rust CLI has provider-specific `run-*-session` commands to execute/resume spawned helper sessions from their own compact event context.
- Python package entry points now hand off to Rust CLI/TUI binaries.
- Python worker package lives under the planned `python/llm_browser_worker` island.
- Local browser-harness helpers load into the worker namespace when available.
- Python worker host helpers stream output, browser state, images, and copied artifacts back to Rust before the final compact response.
- Browser-harness-backed Python actions lazily ensure the daemon and automatically emit current-tab state back to Rust.
- Python image outputs are forwarded to the next model turn as OpenAI-compatible image inputs.
- Durable sub-agent graph foundation: child sessions, agent paths, agent edges, mailbox messages, list, wait, close, and compact parent completion events.
- Child sessions receive a sanitized parent context summary instead of copied parent transcripts.
- TUI activity projection shows helper spawn/completion summaries without copying child transcripts into the parent view.
- Rust fake dataset runner can execute dataset cases into the same SQLite/TUI path.
- Rust OpenAI, Codex, Anthropic, and OpenRouter dataset runners use the same real-provider loop and record dataset case metadata.
- Rust TUI starts the provider loop from the composer with a hidden fake/OpenAI/Codex/Anthropic/OpenRouter backend selector for testing.
- First-run setup, setup-complete, workbench, running, result, stopped, browser, history, actions, help, and developer surfaces have deterministic/manual verification coverage.
- Browser-harness smoke verifies navigation, page inspection, screenshot capture, image artifact indexing, and browser state emission through the Rust/Python boundary.
- Managed headless mode prefers Playwright's bundled testing browser so automated runs do not attach to the user's personal Chrome profile.
- Browser Use cloud mode is owned by the Python browser island when selected and `BROWSER_USE_API_KEY` is available.
- Live testing-browser smoke verifies download artifact indexing and forced stale-session recovery preserving the same browser target id.
- Live Codex count-1 `real_v14_short` dataset smoke passes through FERC search, file download API discovery, PDF/DOCX extraction, and `session.done`.
- First-run setup completion persists in SQLite.
- Deterministic TUI dump tests for setup, ready-with-history, and result.
- `ctrl+c stop` path for running tasks.
- Result follow-ups run against the existing task instead of accidentally creating a new one.
- Setup choices for account, model, provider model, browser, and backend persist in SQLite settings.
- Core records run lifecycle rows, emits `model.config`, `session.status`, and `session.deadline_warning`, defaults provider runs to an 80-turn budget, compacts oversized context with protocol-safe Responses input, and spills huge Python output to artifact-backed tool output.
- CLI includes `config init/show/set`, `auth status`, `auth login`, `auth import-codex`, `auth logout`, `diagnostics`, and trace bundle export.
- Python worker exposes `artifact_root()` and `session_metadata()` to browser task code.
- TUI follows the UX doc vocabulary in the normal product surface.
- Dependencies updated to current stable Rust crates where used: `ratatui 0.30`, `rusqlite 0.39`, `crossterm 0.29`.
- The stale Python `src/llm_browser` product runtime and old Python tests are removed; the Python package surface is now only `llm_browser_worker`.
