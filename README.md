<p align="center">
  <img src="static/browser-use-terminal-banner.png" alt="Browser Use Terminal" />
</p>

# Browser Use Terminal

Automate the boring stuff in the browser.

Browser Use Terminal is a Rust TUI for browser agents. It can use your real browser session, headless Chromium, or Browser Use cloud to get web work done from the terminal.

```bash
curl -fsSL https://raw.githubusercontent.com/browser-use/terminal/main/scripts/install/install.sh | sh
browser
```

<p align="center">
  <img src="static/terminal-preview.png" alt="Browser Use Terminal preview" />
</p>

## What It Does

- runs browser tasks from a terminal UI
- works with logged-in local Chrome
- supports headless and cloud browsers
- lets you watch, steer, stop, and resume tasks
- keeps local history and artifacts

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
