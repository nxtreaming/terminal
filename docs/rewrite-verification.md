# Rust Rewrite Verification

Last verified in this branch:

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
cargo test -p browser-use-store
uv run browser-use-terminal --help
uv run but --help
uv run browser-use-terminal --state-dir /tmp/but-rust-final-smoke run-fake "Open example.com and return ok"
uv run browser-use-terminal --state-dir /tmp/but-rust-dataset-final dataset-run-fake real_v14_short --count 1
uv run browser-use-terminal --state-dir /tmp/but-fake-real-v14-full dataset-run-fake real_v14_short --count 10
uv run browser-use-terminal --state-dir /tmp/but-fake-real-v8-full dataset-run-fake real_v8 --count 100
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-list
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-sample real_v14_short --count 2
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-run-fake real_v14_short --count 2 --run-id audit-smoke
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-report audit-smoke
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-manifest-smoke dataset-run-fake real_v14_short --count 2 --run-id audit-smoke --resume
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-workspace-smoke dataset-run-fake real_v14_short --count 1 --run-id workspace-smoke
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-dataset-workspace-smoke dataset-report workspace-smoke
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-live-smoke run-codex --model gpt-5.5 \
  "Do not use the browser. Call the done tool with result exactly 'ok'."
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-dataset-smoke-cft dataset-run-codex real_v14_short --count 1 --model gpt-5.5
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-real-v14-count2-bounded \
  dataset-run-codex real_v14_short --count 2 --model gpt-5.5 --max-turns 120 --python-timeout-seconds 60
uv run browser-use-terminal --state-dir /tmp/but-rust-cli-config config init
uv run browser-use-terminal --state-dir /tmp/but-rust-cli-config config show
uv run browser-use-terminal auth status
uv run browser-use-terminal --state-dir /tmp/but-auth-smoke auth login openai --api-key test-openai
uv run browser-use-terminal --state-dir /tmp/but-auth-smoke auth login anthropic --api-key test-anthropic
uv run browser-use-terminal --state-dir /tmp/but-auth-smoke auth login openrouter --api-key test-openrouter
uv run browser-use-terminal --state-dir /tmp/but-auth-smoke auth login codex --access-token test-codex-token --account-id test-account
uv run browser-use-terminal --state-dir /tmp/but-auth-claude-code-smoke auth login claude-code --access-token test-oauth-token
uv run browser-use-terminal --state-dir /tmp/but-auth-claude-code-smoke auth status
uv run browser-use-terminal --state-dir /tmp/but-auth-claude-code-smoke config show
uv run browser-use-terminal --state-dir /tmp/but-auth-smoke auth logout openai
uv run browser-use-terminal --state-dir /tmp/but-rust-cli-config diagnostics
```

TUI dump outputs inspected under `/tmp/but-design-loop/`:

- `rust-clean-empty.txt`
- `rust-clean-running.txt`
- `rust-clean-result.txt`
- `rust-clean-browser.txt`
- `rust-clean-history.txt`
- `rust-clean-actions.txt`
- `rust-clean-developer.txt`
- `rust-clean-account.txt`
- `rust-clean-model.txt`
- `rust-clean-stopped.txt`
- `rust-clean-empty-latest.txt`
- `rust-clean-result-latest.txt`
- `rust-clean-browser-latest.txt`
- `rust-clean-history-latest.txt`
- `rust-clean-actions-latest.txt`
- `rust-clean-account-latest.txt`
- `rust-clean-model-latest.txt`
- `rust-clean-developer-latest.txt`

Latest TUI dump outputs inspected under `/tmp/but-design-loop-rust-auth/`:

- `empty.txt`
- `running.txt`
- `result.txt`
- `browser.txt`
- `history.txt`
- `actions.txt`
- `account.txt`
- `model.txt`
- `developer.txt`

Latest deterministic TUI dumps inspected under `/tmp/but-tui-verify/`:

- `setup.txt`
- `account.txt`
- `model.txt`
- `browser.txt`
- `running.txt`
- `result.txt`
- `actions.txt`
- `model-80-after.txt`
- `result-80-after.txt`
- `ready-80-after.txt`

Manual PTY pass:

```bash
uv run but --state-dir /tmp/but-rust-pty-final --seed-demo done --select-latest
uv run but --state-dir /tmp/but-rust-pty-agent --agent fake
uv run but --state-dir /tmp/but-rust-pty-setup-final2 --agent fake
uv run but --state-dir /tmp/but-rust-pty-followup --agent fake
```

Checked `tab`, `f2`, `ctrl+e`, `esc`, `ctrl+q`, setup flow, account/model/browser selection, setup-complete confirmation, task submission, background fake-agent execution, and result rendering.

Latest PTY pass checked first-run setup, account/model/browser selection, setup-complete confirmation, fake task execution, completed-result follow-up on the same task, history `r` resume, actions overlay, help overlay, composer clear, and `ctrl+q` quit. Raw F2 escape injection was not counted in that latest PTY pass because the harness sent bytes that were interpreted as composer text.

Newest PTY pass after stored auth work:

```bash
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-rust-pty-setup-auth --agent fake
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-rust-pty-task-auth
```

Checked first-run setup/account/model/browser flow, fake task submission, done rendering, completed-result follow-up on the same task, history `r` resume, browser overlay via F2 escape sequence, and `ctrl+q` quit. Follow-up evidence is in session `884d565cef70`: it records `session.followup`, a second `model.config`, and a second `session.done`.

Newest PTY pass after the compact-header and first-run model-current fixes:

```bash
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-tui-pty-run
```

Checked an actual 80x24 terminal workbench with stored settings, typed a task into the composer, ran the hidden fake backend, rendered the running state, rendered the done/result state, and exited with `ctrl+q`. Evidence is in session `909304bf92f9`, status `done`, with `session.input`, `model.config`, `model.delta`, and `session.done`.

Final goal PTY pass:

```bash
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-goal-final-tui
```

Checked stored setup settings, 80x24 workbench rendering, task entry, running state, completed result state, history overlay via `tab`, browser overlay via F2 escape sequence, `esc` back to the result, and clean quit with `ctrl+q`. Evidence is in session `5f401d3d9a4f`, status `done`, with `session.input`, `browser.page`, `model.config`, `model.delta`, and `session.done`.

Final browser overlay behavior pass after fixing browser action routing:

```bash
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-goal-browser-overlay-tui
```

Checked stored settings, task submission, completed result rendering, F2 browser overlay, `Open browser` recording `browser.open_requested`, `Reconnect` recording `browser.reconnect_requested`, `Change browser` opening the browser picker instead of mutating the backend directly, selecting `Browser Use cloud`, and clean quit with `ctrl+q`. Evidence is in `/tmp/but-goal-browser-overlay-tui`: app setting `browser=Browser Use cloud`, event `browser.open_requested {"target":"about:blank"}`, and event `browser.reconnect_requested {"browser":"Headless Chromium"}`.

Current branch PTY pass after system-browser avoidance:

```bash
cargo run -q -p browser-use-tui -- --state-dir /tmp/but-current-final-pty --agent fake
```

Checked stored fake-agent settings, task entry, completed result rendering, follow-up execution on the same task, history overlay via `tab`, history `r` resume, browser overlay, and clean quit with `ctrl+q`. Evidence is in session `003d4e98ee60`: it records `session.input`, `browser.page`, `model.config`, `model.delta`, `session.done`, `session.followup`, a second `model.config`, a second `model.delta`, and a second `session.done`. The browser overlay dump reports backend `Headless Chromium`, avoiding both the personal Chrome remote-debug prompt and the quarantined Homebrew Chromium app.

Browser-harness boundary smoke:

```bash
cargo run -q -p browser-use-cli -- --state-dir /tmp/but-rust-harness-smoke python <task-id> \
  'result = {"available": browser_harness_available, "error": browser_harness_error}'

uv run browser-use-terminal --state-dir /tmp/but-rust-harness-nav-smoke python 15fa0f3e92d2 \
  'goto_url("data:text/html,<title>Rust Smoke</title><h1>ok</h1>")
wait_for_load(5)
info = page_info()
shot = capture_screenshot(str(artifact_dir / "smoke.png"), max_dim=1000)
emit_image(shot, label="smoke")
result = {"info": info, "shot": shot}'
```

The worker loaded local browser harness successfully, navigated through browser harness, inspected `page_info()`, captured a screenshot, indexed the image artifact, and emitted `browser.state` from the active browser tab without the old repo-local Python runtime.

Latest live Codex smoke:

- session `dcc8d353b2e9`
- model emitted a `done` tool call
- final `session.done` payload was `{"result":"ok"}`

Latest completion-audit deterministic TUI dumps inspected under `/tmp/but-completion-audit-tui/`:

- `setup.txt`
- `ready.txt`
- `running.txt`
- `result.txt`
- `browser.txt`
- `browser-refresh.txt`
- `history.txt`
- `actions.txt`
- `help.txt`
- `developer.txt`
- `stopped.txt`
- `browser-refresh.txt` shows live browser details as `tabs 1 open` and `viewport 1440 x 900`

Current-branch deterministic TUI dumps inspected under `/tmp/but-current-tui-audit/` after the latest commits:

- `setup.txt`
- `ready.txt`
- `running.txt`
- `result.txt`
- `browser.txt`
- `history.txt`
- `actions.txt`
- `help.txt`
- `developer.txt`
- `stopped.txt`
- `browser.txt` shows useful browser state only: backend, title, page, status, live URL, tabs, and viewport.

Latest bounded real Codex dataset attempt:

- state dir `/tmp/but-rust-codex-real-v14-count2-bounded`
- case `2` pre-created session `8b0c0fa17f59` and recorded `dataset.case` before provider execution
- Python calls were bounded with `--python-timeout-seconds 60`; tool failures were recorded and the agent continued through 114 Python calls
- the attempt stopped on a Codex provider stream error: `cyber_policy`
- the stale-running state exposed by that provider error is now covered by `provider_stream_errors_mark_session_failed_and_finish_run`

Latest fake dataset manifest/report smoke:

- state dir `/tmp/but-dataset-manifest-smoke`
- manifest path `/tmp/but-dataset-manifest-smoke/dataset-runs/audit-smoke.json`
- `dataset-list` resolves `real_v14_short`, `real_v14`, and `real_v8`
- `dataset-sample real_v14_short --count 2` loads cases `2` and `4`
- `dataset-run-fake real_v14_short --count 2 --run-id audit-smoke` wrote two done sessions and summary `passed=2 failed=0 pending=0`
- `dataset-report audit-smoke` returns the compact report with attempts by task
- `dataset-run-fake ... --resume` skips already passed task ids `2` and `4`

Latest fake dataset isolated-workspace smoke:

- state dir `/tmp/but-dataset-workspace-smoke`
- manifest path `/tmp/but-dataset-workspace-smoke/dataset-runs/workspace-smoke.json`
- session `8a8e460ca4b2` used cwd `/tmp/but-dataset-workspace-smoke/dataset-workspaces/workspace-smoke/2-attempt-1`
- summary `passed=1 failed=0 pending=0`

Provider coverage:

- OpenAI Responses, Codex Responses, Anthropic Messages, and OpenAI-compatible chat/OpenRouter paths have mocked HTTP/SSE tests in Rust.
- Anthropic Messages also has mocked bearer-token coverage for the Claude Code OAuth-token path. The CLI stores `auth.claude_code.auth_token`, redacts it in `config show`, detects `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_AUTH_TOKEN`, and reports external Claude Code CLI login status without scraping Keychain tokens.
- Python worker tests cover browser-harness download-style artifacts, refreshed browser target identity across calls, and managed-browser selection that avoids system Chromium unless explicitly opted in.
- Store/core/TUI tests cover run lifecycle rows, model config events, deadline warning events, cancellation checks before finalization, compaction events, large Python output spillover to artifacts, recursive sub-agent close, per-parent sub-agent path uniqueness, configurable spawn fork modes, isolated sub-agent provider execution with compact parent completion events, provider-config execution from core, persistent TUI setup choices, browser overlay action routing, result follow-up execution on the existing task, and Enter-to-retry behavior for failed tasks.
- Store and TUI tests include checked-in golden event fixture import and projection from `tests/golden-events/`, covering legacy done, running/browser-live, artifact/image, and sub-agent/follow-up shapes.
- Core tests cover canonical `/root/...` sub-agent path addressing for send, follow-up, wait, list, and close, author/recipient path envelopes on mailbox events, plus bounded `wait_agent` timeout behavior for active helpers.
- TUI settings, runtime launching, and theme helpers are split into separate modules; `browser-use-tui` does not depend directly on `browser-use-providers`.
- Live Anthropic/OpenRouter smokes were not run in this branch because they require live credentials.

Trace export smoke:

```bash
session=$(uv run browser-use-terminal --state-dir /tmp/but-rust-trace-smoke run-fake "trace smoke")
uv run browser-use-terminal --state-dir /tmp/but-rust-trace-smoke trace "$session" /tmp/but-rust-trace-out
test -f /tmp/but-rust-trace-out/trace.json
```

Managed testing-browser smoke:

```bash
uv run --with playwright python -m playwright install chromium
LLM_BROWSER_BROWSER_MODE=headless uv run browser-use-terminal --state-dir /tmp/but-cft-managed-smoke python <task-id> \
  'goto_url("data:text/html;base64,...")
wait_for_load(5)
fill_input("input[name=docket1]", "CP23-29")
result = {"info": page_info(), "value": js("document.querySelector(\"input[name=docket1]\").value")}'
```

The worker now prefers Playwright's bundled testing browser before any system browser. This avoids both the user's personal Chrome remote-debugging prompt and the quarantined Homebrew Chromium app. The smoke loaded the browser harness, navigated, emitted browser state, and preserved the exact value `CP23-29`.

Live browser boundary smoke:

```bash
scripts/live-browser-boundary-smoke.sh
```

Latest result:

```text
download task: 847e5f8a0ced
stale recovery task: de39aafc9382
state dir: /tmp/but-live-browser-boundary
```

Latest rerun after canonical sub-agent path addressing:

```text
download task: 509b6e952b6b
stale recovery task: 8e23e19e649d
state dir: /tmp/but-live-browser-boundary
```

Latest rerun after stored auth work:

```text
download task: 2de1d2a2cbe1
stale recovery task: 09ea0aff259d
state dir: /tmp/but-live-browser-boundary
```

Latest rerun after deadline warning work:

```text
download task: bc2bd728d817
stale recovery task: 29636382e92c
state dir: /tmp/but-live-browser-boundary
```

Latest rerun after testing-browser selection, browser-harness input fixes, and timeout normalization:

```text
download task: b0d692a284f1
stale recovery task: 8232d5cc4b9a
state dir: /tmp/but-live-browser-boundary
```

This launches an isolated testing-browser CDP target, runs the Rust CLI through the Python worker and browser-harness, verifies browser download artifact indexing, then forces a stale CDP session and verifies recovery keeps the same target id.

Browser-harness fixes:

- `/Users/greg/Developer/browser-harness/src/browser_harness/daemon.py` now reattaches the tracked target before falling back to the first real page.
- `/Users/greg/Developer/browser-harness/src/browser_harness/helpers.py` now uses a single text-carrying keydown for printable input, proper digit/key metadata, and timeout normalization so Playwright-style `timeout=10000` means 10 seconds rather than 10,000 seconds.
- `/Users/greg/Developer/browser-harness/tests/unit/test_daemon.py` and `/Users/greg/Developer/browser-harness/tests/unit/test_helpers.py` cover this behavior.
- Verified with `uv run --with pytest --with pillow --with websockets --with cdp-use --with fetch-use python -m pytest tests/unit -q`.

Real Codex dataset smoke:

```bash
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-dataset-smoke-24 dataset-run-codex real_v14_short --count 1 --model gpt-5.5
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-dataset-smoke-80 dataset-run-codex real_v14_short --count 1 --model gpt-5.5
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-dataset-smoke-80-fixed dataset-run-codex real_v14_short --count 1 --model gpt-5.5
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-dataset-smoke-80-compact-fixed dataset-run-codex real_v14_short --count 1 --model gpt-5.5
uv run browser-use-terminal --state-dir /tmp/but-rust-codex-dataset-smoke-cft dataset-run-codex real_v14_short --count 1 --model gpt-5.5
```

The latest run passed the first `real_v14_short` FERC task:

- state dir: `/tmp/but-rust-codex-dataset-smoke-cft`
- session: `3daba3cc5f52`
- final status: `done`
- final event: `session.done` with summaries for downloaded FERC documents
- evidence: the run used the Rust Codex provider, SQLite event store, Python worker, browser-harness helpers, testing-browser CDP, `fill_input("CP23-29")`, FERC search results, `DownloadP8File`, PDF/DOCX extraction, and dataset metadata event `dataset.case {"dataset":"real_v14_short","task_id":"2"}`

Earlier failed runs were still useful:

- The initial 24-turn default was too low for this task, so the default provider turn budget was raised.
- Longer runs exposed and fixed Codex Responses compaction incompatibility with `system` input messages.
- A later run exposed and fixed compaction retaining old function-call outputs without live matching calls.
- The browser path exposed and fixed Chrome prompt avoidance, printable input event synthesis, and millisecond-style timeout normalization.

Known gaps before calling the whole migration fully complete:

- User-facing config, auth status/login/import/logout, diagnostics, trace commands, API-key auth, Codex import, and Claude Code OAuth-token import exist.
- Full fake dataset path verified: `/tmp/but-fake-real-v14-full` has 10 done sessions and 10 `dataset.case` events; `/tmp/but-fake-real-v8-full` has 100 done sessions and 100 `dataset.case` events.
- Browser Use cloud mode is now owned by the Python island when `LLM_BROWSER_BROWSER_MODE=cloud` and `BROWSER_USE_API_KEY` is set, but it was not live-tested in this branch because no key was available in the environment.
- Full real-provider dataset regression is not green yet. The count-1 Codex smoke on `real_v14_short` passes; a bounded count-2 Codex run made forward progress through the Python/browser path, then stopped on a provider-side `cyber_policy` stream error before completing both cases.
