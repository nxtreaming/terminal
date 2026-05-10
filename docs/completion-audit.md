# Rust Rewrite Completion Audit

This branch is a working Rust-first rewrite foundation, not a claim that every production hardening item is finished.

## Implemented

- Rust workspace split into protocol, store, core, providers, Python-worker supervisor, CLI, and TUI crates.
- SQLite is the durable state boundary for sessions, events, artifacts, runs, agent graph, mailbox, and app settings.
- Checked-in golden legacy events under `tests/golden-events/legacy-session` verify import compatibility for old session/event JSONL shape.
- Old Python product runtime is removed from the package surface; Python is now the browser worker island.
- Python worker loads local browser harness helpers, preserves per-session namespaces, exposes host helpers such as `artifact_root()` and `session_metadata()`, streams host events, and emits browser state/images/artifacts, tab count, and viewport details back to Rust.
- Rust agent loop dispatches the tiny model-visible tool surface: `python`, `done`, `spawn_agent`, `wait_agent`, `send_message`, `followup_task`, `list_agents`, and `close_agent`.
- Child agents are separate sessions with canonical `/root/...` paths, configurable sanitized fork modes, durable graph edges, recursive close/cancel, mailbox messages, and provider execution isolated from the parent transcript.
- Provider adapters exist for fake, OpenAI Responses, Codex Responses, Anthropic Messages, and OpenAI-compatible chat/OpenRouter.
- Claude Code account selection can run Anthropic Messages through a stored or environment OAuth bearer token from `claude setup-token`, `CLAUDE_CODE_OAUTH_TOKEN`, or `ANTHROPIC_AUTH_TOKEN`.
- CLI has task runners, session runners, agent graph commands, import/export, Python tool execution, config, auth status/login/import/logout, diagnostics, trace export, dataset list/sample/report, and resumable dataset runners with isolated per-case workspaces.
- TUI implements the product workbench vocabulary from `docs/terminal-ui-product-ux.md`, including first-run setup, persistent account/model/browser choices, setup-complete, ready, running, result follow-ups, stopped, browser, history, actions, help, and hidden developer views.
- Failed tasks expose a working Enter-to-retry path, and successful retries clear old failure projections from the normal workbench.
- Core emits run lifecycle rows, `session.status`, `model.config`, `session.deadline_warning`, compaction events, compact model contexts, and artifact-backed spillover for huge Python outputs.
- Core checks for external cancellation between provider/tool turns, avoids finalizing cancelled sessions as done, and records cancelled run rows.
- Core records provider stream failures as `session.failed` and closes run rows instead of leaving stale `running` sessions.
- Python tool calls have a configurable timeout; timed-out snippets produce `tool.failed` and the worker namespace remains usable for later calls.
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
- fake dataset runner, including full `real_v14_short` count-10 and `real_v8` count-100 fake paths
- fake dataset manifest/report/resume smoke under `/tmp/but-dataset-manifest-smoke`
- fake dataset isolated-workspace smoke under `/tmp/but-dataset-workspace-smoke`
- checked-in legacy event fixture import test
- live Codex no-browser smoke with a `done` tool call
- config/auth/diagnostics/trace CLI smoke tests
- stored auth CLI smoke for API-key login, Codex token login/import, logout, status, and `config show` secret redaction
- stored Claude Code OAuth-token smoke for login, status, provider credential routing, and `config show` secret redaction
- deterministic TUI dumps for the main product states
- focused TUI regression for retrying a failed task from the failure screen
- manual PTY setup, model/browser selection, task submission, result follow-up, history resume, actions/help, clear input, and quit with the hidden fake backend
- final manual 80x24 PTY smoke with stored settings, task submission, result rendering, history overlay, browser overlay, and clean quit; evidence is in `/tmp/but-goal-final-tui`, session `5f401d3d9a4f`
- final browser-overlay PTY smoke with `Open browser`, `Reconnect`, `Change browser`, browser-picker selection, and clean quit; evidence is in `/tmp/but-goal-browser-overlay-tui`
- browser-harness navigation, page inspection, screenshot artifact, image event, and browser-state emission through the Rust/Python boundary
- worker-boundary tests for browser-harness download-style file artifacts and refreshed browser target identity across calls
- live testing-browser boundary smoke for download artifact indexing and stale-session recovery preserving the same target id
- real Codex count-1 dataset smoke on `real_v14_short` passed through the Rust provider loop, Python tool, SQLite store, testing-browser CDP path, FERC search, FERC file download API, PDF/DOCX extraction, and final `session.done`
- bounded real Codex count-2 dataset attempt pre-created durable dataset sessions and exercised timeout/error continuation, then stopped on a Codex `cyber_policy` stream error before the first case finished
- earlier real Codex count-1 dataset attempts exposed two compaction protocol bugs and browser-harness input/timeout issues that are now covered by focused tests

## Remaining Gaps

- Live Anthropic/OpenRouter smokes were not run.
- Browser Use cloud mode was not live-tested because no `BROWSER_USE_API_KEY` was available in the environment.
- Full real-provider dataset regression is not green yet. The count-1 Codex run on `real_v14_short` passes; the count-2 Codex attempt is blocked by a provider-side `cyber_policy` stream error rather than a local Rust/Python deadlock.
