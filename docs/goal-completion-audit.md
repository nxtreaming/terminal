# Goal Completion Audit

Date: 2026-05-10

Objective audited:

- Implement the Rust-first migration plan from `/Users/greg/Documents/browser-use/experiments/llm-browser-rust-tui/docs/rust-core-migration-plan.md`.
- Implement the terminal UI product UX from `/Users/greg/Documents/browser-use/experiments/llm-browser-rust-tui/docs/terminal-ui-product-ux.md`.
- Test the terminal UI manually end to end.

## Checklist

| Requirement | Evidence | Status |
| --- | --- | --- |
| Rust owns durable state, orchestration, providers, model/tool scheduling, CLI, and TUI. | Workspace crates `browser-use-protocol`, `browser-use-store`, `browser-use-core`, `browser-use-providers`, `browser-use-cli`, and `browser-use-tui`; old Python product runtime removed from package surface; TUI no longer constructs concrete provider adapters. | Done |
| Python remains the browser island and understands live browser reconnect/identity. | `python/llm_browser_worker/worker.py` loads browser-harness helpers, emits browser state/artifacts, supports managed testing-browser and Browser Use cloud modes; browser-harness fixes committed in `/Users/greg/Developer/browser-harness`. | Done |
| SQLite replaces per-session JSON files as primary state. | `crates/browser-use-store` migrations create `sessions`, `events`, `artifacts`, `runs`, `agent_edges`, `agent_messages`, and `app_settings`; tests cover store/session/events/import/export and checked-in golden event fixtures. | Done |
| Append-only events are the integration contract. | Core/store append normalized `session.*`, `model.*`, `tool.*`, `browser.*`, `agent.*`, artifact, and compaction events; TUI projects `WorkbenchState` from events. | Done |
| Tool surface stays small and browser-agent shaped. | Core exposes model tools `python`, `done`, `spawn_agent`, `wait_agent`, `send_message`, `followup_task`, `list_agents`, and `close_agent`; no model-visible file editing or shell tools. | Done |
| Codex-shaped sub-agents prevent parent context blowups. | Durable `agent_edges` and `agent_messages`; canonical `/root/...` paths with per-parent uniqueness enforced by SQLite; sanitized fork modes; bounded wait/list/message/followup/close tools; mailbox events include author/recipient paths; spawned/follow-up children now execute as isolated provider sessions; tests cover no child transcript copying and compact parent completion events. | Done |
| Browser mutations go through Python browser harness, not Rust CDP. | Rust supervises the Python worker only; worker uses browser-harness/admin/helpers for browser operations; Rust records events and artifacts. | Done |
| Providers cover fake, Codex, OpenAI, Anthropic, and OpenRouter. | Provider crate has fake, OpenAI Responses, Codex Responses, Anthropic Messages, and OpenAI-compatible chat/OpenRouter adapters with mocked HTTP/SSE tests. | Done |
| Claude Code path exists. | `auth login claude-code --access-token ...`, `CLAUDE_CODE_OAUTH_TOKEN`, and `ANTHROPIC_AUTH_TOKEN` route to Anthropic bearer-token auth with redaction and tests. | Done |
| Cancellation is runtime-visible. | Store records cancel events/status; core now checks cancellation before finalizing model/tool turns and records cancelled run rows; `provider_loop_respects_external_cancel_before_finalizing` covers this. | Done |
| Dataset runner is ported. | Fake/OpenAI/Codex/Anthropic/OpenRouter dataset runner commands exist; dataset list/sample/report commands exist; runs write resumable manifests under `state_dir/dataset-runs` and per-case workspaces under `state_dir/dataset-workspaces`; fake `real_v14_short` count-10 and fake `real_v8` count-100 passed; Codex count-1 `real_v14_short` passed. | Mostly done |
| Terminal UI first-run setup matches the UX doc. | TUI has setup, sign-in, model, browser choice, setup-complete, persisted choices, and tests for setup flow. | Done |
| Terminal UI workbench matches the UX doc vocabulary. | Normal TUI uses `task/browser/account/model/result/history/setup` vocabulary, hides artifact/trace/provider/event concepts behind developer overlay, and keeps settings/runtime/theme concerns outside the main render file. | Done |
| Terminal UI running/result/history/actions/browser/failure behavior works. | Unit tests plus manual PTY runs cover task entry, running, result, follow-up, failure retry, history, actions, help, browser overlay, browser picker, and quit. | Done |
| Browser overlay actions do what they say. | Fixed and tested: `Open browser` records `browser.open_requested`, `Reconnect` records `browser.reconnect_requested`, `Change browser` opens browser picker without accidental backend mutation; live browser state now projects tab count and viewport details instead of hardcoded unknowns. | Done |
| Terminal UI was manually tested. | `/tmp/but-goal-final-tui` and `/tmp/but-goal-browser-overlay-tui` PTY runs inspected; current deterministic dumps under `/tmp/but-current-tui-audit` cover setup, ready, running, result, browser, history, actions, help, developer, and stopped states; SQLite evidence recorded in `docs/rewrite-verification.md`. | Done |
| Rust package choices are current/reasonable. | Current checks found `ratatui 0.30.0`, `rusqlite 0.39.0`, `reqwest 0.12.x`, `sqlx 0.8.6`, `async-openai v0.38.0`, and `eventsource-client 0.17.3` as available options. The implementation uses Ratatui, rusqlite, and thin reqwest adapters where SDK coverage is not worth extra complexity. | Done |
| Provider failures leave durable failed state. | Core records `session.failed` and closes the run row when a provider stream errors; regression test `provider_stream_errors_mark_session_failed_and_finish_run` covers the path exposed by a live Codex `cyber_policy` stream error. | Done |
| Python tool execution is bounded. | Core passes a Python timeout to the worker; worker uses an alarm around snippet execution and preserves the session namespace after timeout; tests cover timeout recovery and model continuation. | Done |

## Verification Commands

Latest local verification:

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
cargo test -p browser-use-core -p browser-use-tui
cargo test -p browser-use-cli -p browser-use-protocol -p browser-use-tui -p browser-use-python-worker
cargo test -p browser-use-store
uv run --with pytest python -m pytest -q
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-list
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-sample real_v14_short --count 2
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-run-fake real_v14_short --count 2 --run-id audit-smoke
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-report audit-smoke
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-run-fake real_v14_short --count 2 --run-id audit-smoke --resume
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-workspace-smoke dataset-run-fake real_v14_short --count 1 --run-id workspace-smoke
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-workspace-smoke dataset-report workspace-smoke
uv run browser-use-terminal --state-dir /tmp/but-fake-real-v14-full dataset-run-fake real_v14_short --count 10
uv run browser-use-terminal --state-dir /tmp/but-fake-real-v8-full dataset-run-fake real_v8 --count 100
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-real-v14-count2-bounded \
  dataset-run-codex real_v14_short --count 2 --model gpt-5.5 --max-turns 120 --python-timeout-seconds 60
```

Previously recorded live/browser verification:

```bash
scripts/live-browser-boundary-smoke.sh
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-live-smoke-final run-codex --model gpt-5.5 \
  "Do not use the browser. Call the done tool with result exactly 'ok'."
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-dataset-smoke-cft dataset-run-codex real_v14_short --count 1 --model gpt-5.5
```

## Remaining Gaps

These are not implementation gaps in the local rewrite, but they prevent a strict claim that every production path has been live-validated:

- Live Anthropic and OpenRouter smokes were not run because live credentials were not available.
- Browser Use cloud mode was not live-tested because `BROWSER_USE_API_KEY` was not available.
- Full real-provider dataset regression is not green yet. The count-1 Codex `real_v14_short` smoke passed; a count-2 Codex attempt with bounded Python tools recorded forward progress and durable `dataset.case` metadata, then stopped on a Codex `cyber_policy` stream error before case 1 completed.

## Package Research Notes

- Ratatui latest docs show `0.30.0`: https://docs.rs/crate/ratatui/latest
- Rusqlite latest docs show `0.39.0`: https://docs.rs/crate/rusqlite/latest
- SQLx docs show SQLite migrations support, but the current synchronous local app shape favors smaller `rusqlite`: https://docs.rs/sqlx/latest/sqlx/migrate/index.html
- `async-openai` latest docs show `0.38.0`, but the implementation keeps thin `reqwest` adapters to support Codex/Responses differences directly: https://docs.rs/crate/async-openai/latest
- Anthropic Messages API is simple enough for a thin adapter and supports tool/message usage directly: https://docs.claude.com/en/api/messages
- `eventsource-client` latest docs show `0.17.3`, but the current blocking provider adapter uses a small local SSE parser to avoid pulling async transport into the v1 runtime: https://docs.rs/eventsource-client/latest/eventsource_client/
