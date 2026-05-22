<p align="center">
  <img src="static/browser-use-terminal-banner.png" alt="Browser Use Terminal" />
</p>

# Browser Use Terminal

Automate the boring stuff in the browser.

Browser Use Terminal is a Rust TUI for browser agents. It combines a new LLM harness, Browser Harness-style CDP control, real Chrome sessions, and a terminal UI you can actually steer.

```bash
curl -fsSL https://browser-use.com/terminal/install.sh | sh
browser
```

<p align="center">
  <img src="static/terminal-preview.png" alt="Browser Use Terminal preview" />
</p>

## What It Does

- runs browser tasks from a terminal UI
- works with your logged-in Chrome when the task needs real account state
- supports headless Chromium and Browser Use cloud for clean or remote runs
- lets you watch, steer, stop, retry, and resume tasks
- keeps local history, screenshots, artifacts, and follow-ups
- uses a new LLM harness built to be 2x cheaper and 2x faster than Browser Harness

## How It Works

```text
┌─────────────────────────────────────────────────────────────┐
│                         browser                             │
├─────────────────────────────────────────────────────────────┤
│  terminal UI                                                │
│  custom Ratatui renderer, native scrollback, keyboard UX     │
├─────────────────────────────────────────────────────────────┤
│  Rust LLM harness                                           │
│  provider loop, tools, subagents, compaction, cancellation   │
├─────────────────────────────────────────────────────────────┤
│  event store                                                │
│  SQLite history, artifacts, screenshots, follow-ups, traces  │
├─────────────────────────────────────────────────────────────┤
│  browser runtime                                            │
│  connect, profiles, doctor, recovery, ownership             │
├─────────────────────────────────────────────────────────────┤
│  browser interaction                                        │
│  direct CDP, page JS, screenshots, files, helper workspace   │
├─────────────────────────────────────────────────────────────┤
│  Chrome                                                     │
│  real logged-in Chrome, headless Chromium, or cloud browser  │
└─────────────────────────────────────────────────────────────┘
```

The harness is built from scratch around browser work. The model gets a CDP-first action space instead of a narrow framework API, while Rust keeps the durable state: task history, tool calls, artifacts, browser status, cancellation, follow-ups, and subagents.

The TUI is not a log dump. It has a custom Ratatui renderer that keeps completed output in native terminal scrollback, renders only live state in the active viewport, and drives real keyboard flows for history, browser controls, model/auth setup, steering, stopping, and retrying.

Browser state is explicit: connect, profile, doctor, recover, stop. No mystery browser session hiding behind the agent loop.

## Try It

```text
Get my San Francisco parking permit.
```

```text
Give this employee admin permission in Azure.
```

```text
Find the cancellation policy for my current hotel reservation.
```

## Setup

Launch the app:

```bash
browser
```

Use slash commands inside the TUI:

```text
/auth      sign in
/model     choose a model
/browser   choose local, headless, or cloud browser
/update    update the app
```

Useful shell commands:

```bash
browser auth status
browser config show
browser diagnostics
```

## Development

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
scripts/verify-terminal-ui.sh
```

Terminal UI changes must pass the full verification script. It runs Rust tests, Python tests, deterministic Ratatui dumps, and a real tmux smoke test.

## Docs

- `docs/terminal-ui-product-ux.md`
- `docs/terminal-ui-testing.md`
- `docs/terminal-renderer-architecture.md`

## License

MIT
