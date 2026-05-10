# Agent Notes

## Rust Rewrite Verification Loop

This branch is Rust-first. The old Textual app and `src/llm_browser` runtime are intentionally gone.

Use these checks after runtime, state, provider, worker, or TUI changes:

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
```

Use deterministic Ratatui dumps for visual iteration:

```bash
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-rust-empty --dump-screen
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-rust-ready --seed-demo done --select-latest --dump-screen
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-rust-running --seed-demo running --select-latest --dump-screen
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-rust-browser --seed-demo done --select-latest --overlay browser --dump-screen
```

Keep any saved dump outputs under `/tmp/but-design-loop/`.

For manual terminal behavior, run the Rust TUI in a PTY:

```bash
uv run but --seed-demo done
```

Useful keys to verify: `tab`, `f1`, `f2`, `ctrl+e`, `ctrl+c`, `ctrl+q`, `enter`, `esc`, and arrow keys inside overlays.

Before calling the TUI polished, inspect setup, ready, running, result, browser overlay, history overlay, actions overlay, help overlay, developer overlay, and stopped-task states.
