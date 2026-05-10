# Rust Rewrite Completion Audit

This branch is a working Rust-first rewrite foundation, not a claim that every production hardening item is finished.

## Implemented

- Rust workspace split into protocol, store, core, providers, Python-worker supervisor, CLI, and TUI crates.
- SQLite is the durable state boundary for sessions, events, artifacts, runs, agent graph, mailbox, and app settings.
- Old Python product runtime is removed from the package surface; Python is now the browser worker island.
- Python worker loads local browser harness helpers, preserves per-session namespaces, exposes host helpers such as `artifact_root()` and `session_metadata()`, streams host events, and emits browser state/images/artifacts back to Rust.
- Rust agent loop dispatches the tiny model-visible tool surface: `python`, `done`, `spawn_agent`, `wait_agent`, `send_message`, `followup_task`, `list_agents`, and `close_agent`.
- Child agents are separate sessions with canonical `/root/...` paths, configurable sanitized fork modes, durable graph edges, recursive close/cancel, and mailbox messages.
- Provider adapters exist for fake, OpenAI Responses, Codex Responses, Anthropic Messages, and OpenAI-compatible chat/OpenRouter.
- CLI has task runners, session runners, agent graph commands, import/export, Python tool execution, config, auth status/login/import/logout, diagnostics, trace export, and dataset runners.
- TUI implements the product workbench vocabulary from `docs/terminal-ui-product-ux.md`, including first-run setup, persistent account/model/browser choices, setup-complete, ready, running, result follow-ups, stopped, browser, history, actions, help, and hidden developer views.
- Core emits run lifecycle rows, `session.status`, `model.config`, `session.deadline_warning`, compaction events, compact model contexts, and artifact-backed spillover for huge Python outputs.
- Default provider runs allow up to 80 turns; compacted Responses input converts summarized system context to user context and avoids replaying stale historical function-call outputs.
- Managed headless browser mode is owned by the Python island and prefers Playwright's bundled testing browser, avoiding the user's personal Chrome remote-debugging prompt and quarantined system Chromium apps.
- Browser Use cloud mode is also owned by the Python island when selected and `BROWSER_USE_API_KEY` is available.

## Verified

- `cargo fmt --check`
- `cargo test`
- `uv run --with pytest python -m pytest -q`
- `scripts/live-browser-boundary-smoke.sh`
- `uv run --with pytest --with pillow --with websockets --with cdp-use --with fetch-use python -m pytest tests/unit/test_daemon.py -q` in `/Users/greg/Developer/browser-harness`
- `uv run browser-use-terminal --help`
- `uv run but --help`
- fake CLI task runner
- fake dataset runner
- live Codex no-browser smoke with a `done` tool call
- config/auth/diagnostics/trace CLI smoke tests
- stored auth CLI smoke for API-key login, Codex token login/import, logout, status, and `config show` secret redaction
- deterministic TUI dumps for the main product states
- manual PTY setup, model/browser selection, task submission, result follow-up, history resume, actions/help, clear input, and quit with the hidden fake backend
- browser-harness navigation, page inspection, screenshot artifact, image event, and browser-state emission through the Rust/Python boundary
- worker-boundary tests for browser-harness download-style file artifacts and refreshed browser target identity across calls
- live dedicated-Chrome boundary smoke for download artifact indexing and stale-session recovery preserving the same target id
- real Codex count-1 dataset smoke on `real_v14_short` passed through the Rust provider loop, Python tool, SQLite store, testing-browser CDP path, FERC search, FERC file download API, PDF/DOCX extraction, and final `session.done`
- earlier real Codex count-1 dataset attempts exposed two compaction protocol bugs and browser-harness input/timeout issues that are now covered by focused tests

## Remaining Gaps

- API-key and Codex credential login/import/logout are implemented, but true Claude Code OAuth/import is still not implemented.
- Live Anthropic/OpenRouter smokes were not run.
- Browser Use cloud mode was not live-tested because no `BROWSER_USE_API_KEY` was available in the environment.
- Full real-provider dataset regression has not been run. The count-1 Codex run on `real_v14_short` now passes.
