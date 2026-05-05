# browser use terminal implementation status

This file records the current evidence for the harness implementation.

## Implemented

- Product/CLI surface renamed to `browser use terminal`.
- `uv` manages dependencies and commands.
- Codex subscription provider defaults to `gpt-5.5`.
- Raw CDP browser runtime with reconnect, tab helpers, navigation, screenshots, visible text, links, and full-page capture.
- Persistent Python browser tool with raw CDP helpers, workspace cwd isolation, artifact helpers, requests/BeautifulSoup/pandas/PdfReader/Pillow preload, and model-visible screenshot attachments.
- Shell/file tools, including unified-diff patch application.
- Recoverable tool errors.
- Absolute state paths so tool cwd changes cannot corrupt event storage.
- Event-driven background session manager and Textual TUI.
- Session cancellation, trace bundling, resume, compaction, and self-eval child sessions.
- Dataset list/sample/run commands for `real_v8` and `real_v14_short`.
- Isolated dataset workspaces under `.browser-use-terminal/dataset-runs/...`.

## Verification

- Unit tests: `uv run python -m unittest discover -s tests` passes with 34 tests.
- Browser smoke: `uv run browser-use-terminal browser smoke --headless --url https://example.com` passes.
- Fake dataset smoke: `uv run browser-use-terminal datasets run real_v8 --provider fake --count 1 --seed 3` passes.
- Real `gpt-5.5` runs:
  - `real_v8` task 34: session `940ce19a2ef4`, completed.
  - `real_v14_short` task 9: session `a4c4517fd58d`, completed; output image visually verified.
  - `real_v8` task 22: session `eedd29928174`, completed.

## Known Remaining Work

- Not every task in both datasets has been executed yet.
- TUI is functional and much better structured, but can still be polished with richer previews and keybindings.
- Chrome profile internals still exist on disk, though trace/TUI artifact listings filter them out.
- Resume reconstructs trace history approximately; arbitrary mid-tool resume still needs deeper provider/tool state recovery.
