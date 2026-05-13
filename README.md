# browser-use terminal

Rust-first browser agent workbench.

The repository is now shaped around one rule:

```text
Rust owns durable product state.
Python owns volatile browser state.
Events are the contract.
```

## What Runs Where

Rust owns:

- terminal UI and CLI
- SQLite state and migrations
- append-only event log
- agent loop, cancellation, resume, and sub-agent graph
- provider adapters
- dataset runner

Python owns:

- the browser-connected execution namespace
- browser-harness helper loading
- CDP/session/target identity through browser harness
- scraping, image, PDF, table, and browser helper libraries

The old Python product runtime has been removed from the package surface. The only Python package left is `llm_browser_worker`, which is launched by Rust for browser/tool execution.

## Repository Shape

```text
crates/
  browser-use-protocol/       shared event/tool/model types and projections
  browser-use-store/          SQLite store, migrations, artifacts, agent graph
  browser-use-core/           provider-driven agent loop and tool dispatch
  browser-use-providers/      fake, OpenAI, Codex, Anthropic, OpenRouter
  browser-use-python-worker/  Rust supervisor for the Python worker process
  browser-use-cli/            CLI, datasets, diagnostics
  browser-use-tui/            Ratatui product workbench

python/
  llm_browser_worker/         persistent browser Python worker island
```

Runtime state lives under:

```text
.browser-use-terminal/
  state.db
  artifacts/
```

## Common Commands

```bash
cargo test
uv run --with pytest python -m pytest -q

cargo run -p browser-use-tui -- --help
cargo run -p browser-use-cli -- --help

cargo run -p browser-use-tui -- --seed-demo done
cargo run -p browser-use-cli -- run-fake "Open example.com and report the title"
cargo run -p browser-use-cli -- run-openai "Open example.com and report the title"
cargo run -p browser-use-cli -- run-codex "Open example.com and report the title"
cargo run -p browser-use-cli -- run-anthropic "Open example.com and report the title"
cargo run -p browser-use-cli -- run-openrouter "Open example.com and report the title"

cargo run -p browser-use-cli -- config init
cargo run -p browser-use-cli -- config show
cargo run -p browser-use-cli -- auth status
cargo run -p browser-use-cli -- auth login openai --api-key "$OPENAI_API_KEY"
cargo run -p browser-use-cli -- auth import-codex --from ~/.codex/auth.json
cargo run -p browser-use-cli -- auth login claude-code --access-token "$CLAUDE_CODE_OAUTH_TOKEN"
cargo run -p browser-use-cli -- auth logout openai
cargo run -p browser-use-cli -- diagnostics
cargo run -p browser-use-cli -- trace <task-id> /tmp/browser-use-trace
scripts/live-browser-boundary-smoke.sh
```

For the clean binary names locally:

```bash
cargo build
./target/debug/but
./target/debug/browser-use-terminal auth status
```

Provider auth:

- OpenAI Responses uses stored `auth login openai`, `LLM_BROWSER_OPENAI_API_KEY`, or `OPENAI_API_KEY`.
- Codex Responses uses stored `auth login codex` / `auth import-codex`, `LLM_BROWSER_CODEX_ACCESS_TOKEN` + `LLM_BROWSER_CODEX_ACCOUNT_ID`, or `~/.codex/auth.json`.
- Anthropic Messages uses stored `auth login anthropic`, `LLM_BROWSER_ANTHROPIC_API_KEY`, or `ANTHROPIC_API_KEY`.
- Claude Code subscription uses a stored OAuth token from `auth login claude-code --access-token ...`, `CLAUDE_CODE_OAUTH_TOKEN`, or `ANTHROPIC_AUTH_TOKEN`. Use `claude setup-token` to create a long-lived token.
- OpenRouter/OpenAI-compatible chat uses stored `auth login openrouter`, `LLM_BROWSER_OPENAI_COMPAT_API_KEY`, or `OPENROUTER_API_KEY`.

`auth status` reports the currently usable auth paths. `config show` redacts stored API keys and access tokens.

Optional Laminar tracing:

- Set `LMNR_PROJECT_API_KEY` to emit one OpenTelemetry trace per agent session.
- LLM turns are exported as Laminar `LLM` spans using the same Laminar/OpenLLMetry attribute shape as the TypeScript SDK: `lmnr.span.input`, `lmnr.span.output`, `lmnr.span.path`, `gen_ai.prompt.*`, `gen_ai.completion.*`, tool-call attributes, and token usage when the provider returns usage.
- The default OTLP/HTTP/proto endpoint is `https://api.lmnr.ai/v1/traces`; override it with `LLM_BROWSER_LAMINAR_OTLP_ENDPOINT` or set `OTEL_EXPORTER_OTLP_ENDPOINT` for self-hosted Laminar.
- Set `LLM_BROWSER_LAMINAR_CAPTURE_PAYLOADS=0` to keep only metadata, or tune `LLM_BROWSER_LAMINAR_MAX_ATTR_CHARS` when payload attributes are too large.
- Telemetry is fail-open: exporter setup errors disable telemetry for the run, export happens on a background batch thread, and end-of-run flushing is off by default. For one-shot CLI smoke tests, set `LLM_BROWSER_LAMINAR_FLUSH_ON_FINISH=1`.
- Local session events include `telemetry.trace` with the Laminar trace id and endpoint.
- The TUI shows the Laminar trace id on the current task when telemetry is present. Use `/` -> `Developer trace` or `ctrl+e` for trace endpoint and raw telemetry events.

## Product UI

The TUI follows `docs/terminal-ui-product-ux.md`.

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

Developer details such as raw events, tools, provider internals, and artifacts stay behind the hidden developer/debug surface.

Deterministic TUI dump examples:

```bash
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-ui --dump-screen
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-ui --seed-demo running --select-latest --dump-screen
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-ui --seed-demo done --select-latest --overlay browser --dump-screen
```

Before landing terminal UI changes, run the real verification loop:

```bash
scripts/verify-terminal-ui.sh
```

It writes deterministic dumps to `/tmp/but-design-loop/` and drives the TUI through a real tmux terminal to catch scrollback, paste, arrow-key, redraw, and selectable-output regressions.

## Browser Boundary

The Python worker tries to load local browser-harness helpers from:

```text
/Users/greg/Developer/browser-harness/src
```

or from `BROWSER_HARNESS_SRC`.

When browser harness is available, browser actions run through that Python daemon shape. Rust records browser-visible facts as events, but Rust does not own CDP target ids, session ids, runtime object ids, or reconnect recovery.

The current smoke path verifies browser-harness navigation, page inspection, screenshot capture, image artifact indexing, and `browser.state` emission through the Rust CLI/Python worker boundary.

For a live browser boundary regression with an isolated headless Chrome:

```bash
scripts/live-browser-boundary-smoke.sh
```

That smoke covers browser download artifact indexing and stale-session recovery preserving the same browser target id.

## Migration Docs

- `docs/rust-migration-one-page-summary.md`
- `docs/rust-core-migration-plan.md`
- `docs/terminal-ui-product-ux.md`
- `docs/browser-agent-harness-learnings.md`
- `docs/browser-agent-harness-plan.md`
- `docs/rewrite-verification.md`
- `docs/completion-audit.md`
- `docs/goal-completion-audit.md`
