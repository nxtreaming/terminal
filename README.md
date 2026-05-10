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

uv run but --help
uv run browser-use-terminal --help

uv run but --seed-demo done
uv run browser-use-terminal run-fake "Open example.com and report the title"
uv run browser-use-terminal run-openai "Open example.com and report the title"
uv run browser-use-terminal run-codex "Open example.com and report the title"
uv run browser-use-terminal run-anthropic "Open example.com and report the title"
uv run browser-use-terminal run-openrouter "Open example.com and report the title"

uv run browser-use-terminal config init
uv run browser-use-terminal config show
uv run browser-use-terminal auth status
uv run browser-use-terminal auth login openai --api-key "$OPENAI_API_KEY"
uv run browser-use-terminal auth import-codex --from ~/.codex/auth.json
uv run browser-use-terminal auth logout openai
uv run browser-use-terminal diagnostics
uv run browser-use-terminal trace <task-id> /tmp/browser-use-trace
scripts/live-browser-boundary-smoke.sh
```

Provider auth:

- OpenAI Responses uses stored `auth login openai`, `LLM_BROWSER_OPENAI_API_KEY`, or `OPENAI_API_KEY`.
- Codex Responses uses stored `auth login codex` / `auth import-codex`, `LLM_BROWSER_CODEX_ACCESS_TOKEN` + `LLM_BROWSER_CODEX_ACCOUNT_ID`, or `~/.codex/auth.json`.
- Anthropic Messages uses stored `auth login anthropic`, `LLM_BROWSER_ANTHROPIC_API_KEY`, or `ANTHROPIC_API_KEY`.
- OpenRouter/OpenAI-compatible chat uses stored `auth login openrouter`, `LLM_BROWSER_OPENAI_COMPAT_API_KEY`, or `OPENROUTER_API_KEY`.

`auth status` reports the currently usable auth paths. `config show` redacts stored API keys and access tokens.

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
