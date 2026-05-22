# Contributing

Thanks for wanting to contribute. Browser Use Terminal is early, and the contribution process is still being shaped.

For now:

- Open an issue before large changes so we can align on scope.
- Keep PRs small and focused.
- Run the relevant checks before opening a PR:

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
```

For terminal UI changes, also run:

```bash
scripts/verify-terminal-ui.sh
```

More detailed contribution guidelines are coming soon.
