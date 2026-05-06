# browser use terminal implementation status

This file records the current evidence for the harness implementation.

## Implemented

- Product/CLI surface renamed to `browser use terminal`.
- `uv` manages dependencies and commands.
- Codex subscription provider defaults to `gpt-5.5`.
- Raw CDP browser runtime with reconnect, tab helpers, navigation, screenshots, visible text, links, and full-page capture.
- Persistent Python browser tool with raw CDP helpers, workspace cwd isolation, artifact helpers, requests/BeautifulSoup/pandas/PdfReader/Pillow preload, and model-visible screenshot attachments.
- Shell/file tools, including streamed shell output events, cancellation, and unified-diff patch application.
- Recoverable tool errors.
- Absolute state paths so tool cwd changes cannot corrupt event storage.
- Event-driven background session manager and Textual TUI with session detail, artifact sizes/preview, trace export, self-eval start, resume, and cancellation.
- Session cancellation, trace bundling, resume, compaction, and self-eval child sessions.
- Dataset list/sample/run commands for `real_v8` and `real_v14_short`.
- Isolated dataset workspaces under `.browser-use-terminal/dataset-runs/...`.
- Dataset resume/report semantics use latest attempts, so a successful rerun supersedes an older failed attempt.

## Verification

- Unit tests: `uv run python -m unittest discover -s tests` passes with 42 tests.
- Browser smoke: `uv run browser-use-terminal browser smoke --headless --url https://example.com` passes.
- Fake dataset smoke: `uv run browser-use-terminal datasets run real_v8 --provider fake --count 1 --seed 3` passes.
- Real `gpt-5.5` runs:
  - `real_v8` task 34: session `940ce19a2ef4`, completed.
  - `real_v14_short` task 9: session `a4c4517fd58d`, completed; output image visually verified.
  - `real_v8` task 22: session `eedd29928174`, completed.
  - `real_v14_short` full run `real-v14-gpt55-full`: 10 selected, 10 passed, 0 failed. Task 11 passed on a latest rerun after using fallback evidence from `fccid.io`.
  - `real_v8` full run `real-v8-gpt55-full`: currently running.

## Known Remaining Work

- Not every `real_v8` task has been executed and reviewed yet.
- TUI is much more usable, but still needs a real terminal visual pass after the full dataset run and could use inline image rendering in terminals that support it.
- Chrome profile internals still exist on disk, though trace/TUI artifact listings filter them out.
- Resume reconstructs trace history approximately; arbitrary mid-tool resume still needs deeper provider/tool state recovery.
