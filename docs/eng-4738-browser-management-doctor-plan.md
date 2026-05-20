# ENG-4738 Browser Management + Doctor Plan

## Summary

Implement a Rust-owned browser control plane with a small explicit LLM surface:

- `browser`: CLI-style control/debug tool for connect/status/doctor/recovery/runtime management.
- `browser_script`: fresh Python execution tool for page interaction with browser helpers preimported.
- `view_image`: local image inspection tool documented as sequential and not parallel-safe.

Worktree: `/Users/greg/Documents/browser-use/experiments/llm-browser-eng-4738-browser-control`

Branch: `gregor/eng-4738-browser-management-doctor`

## Checklist

- [ ] Commit this plan before implementation.
- [ ] Add Rust browser runtime in `crates/browser-use-browser`.
- [ ] Add `browser` CLI-style tool and README-like tool description.
- [ ] Add `browser_script` fresh Python runner and helper preload.
- [ ] Move browser connection ownership out of persistent Python state.
- [ ] Support local attach, managed browser, remote CDP, and Browser Use cloud start/connect.
- [ ] Add status, doctor, recovery, runtime logs, ownership, and stale cleanup commands.
- [ ] Mark `view_image` sequential/not parallel-safe.
- [ ] Update TUI/settings/state to reflect local attach vs managed launch.
- [ ] Add unit/integration tests for parser, status, doctor, recovery, and scripting.
- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo test`.
- [ ] Run `uv run --with pytest python -m pytest -q`.
- [ ] Run `scripts/verify-terminal-ui.sh` for TUI coverage.
- [ ] Run bounded real-LLM smoke test if credentials are available; otherwise record blocker.

## Interface

`browser` accepts one raw command string. It does not click, type, scrape, screenshot, or run page JavaScript.

Commands:

- `help`
- `status --json`
- `doctor` / `doctor --json`
- `connect local`
- `connect local --candidate <id>`
- `connect managed [--headless|--headed] [--profile temp|<path>]`
- `connect remote-cdp --url <http-url>`
- `connect remote-cdp --ws <ws-url>`
- `local list --json`
- `local setup`
- `local profiles --json`
- `local profiles inspect <profile-name> --domains-only`
- `remote start [--profile-id <uuid>|--profile-name <name>]`
- `remote stop`
- `remote status --json`
- `remote live-url`
- `remote profiles --json`
- `recover reconnect-websocket`
- `recover reattach-same-target`
- `recover restart-runtime`
- `recover restart-owned-browser`
- `recover stop-owned-remote`
- `runtime logs`
- `runtime ownership --json`
- `runtime cleanup-stale`

`browser_script` runs fresh Python per call. Browser/CDP state persists in Rust. Python variables do not persist.

Preimported helpers include:

- `cdp`, `cdp_batch`, `js`
- `goto_url`, `new_tab`, `page_info`
- `screenshot`, `screenshot_clip`, `capture_screenshot`
- `click_at_xy`, `type_text`, `press_key`, `scroll`
- `wait_for_load`, `wait_for_element`, `wait_for_network_idle`
- `current_tab`, `list_tabs`, `switch_tab`, `ensure_real_tab`
- `upload_file`, `drain_events`
- `copy_artifact`, `emit_image`, `set_final_answer`, `audit_artifact`

## Scope

Included:

- Rust-held CDP websocket/session/target state.
- Explicit local attach to already-running browser.
- Explicit managed browser launch using temp or non-default profile.
- Browser Use cloud start/connect/stop.
- Read-only doctor with exact next commands.
- Explicit recovery commands that do not reload pages silently.

Deferred:

- Local-to-cloud profile sync.
- Copying real Chrome profiles.
- Tab locks and automatic tab cleanup.
- Network recorder/HAR.
- Auth/TOTP/secrets tooling.
- Custom browser family configuration UI.

## Test Plan

- Rust unit tests for command parsing, status JSON, ownership safety, doctor output, and recovery eligibility.
- Mock CDP tests for websocket drop, stale session, target gone, unreachable endpoint, and multiple local candidates.
- Managed browser integration smoke for connect, page inspection, screenshot artifact, and owned shutdown.
- Local browser doctor test against the host without mutating Chrome.
- Mock remote-cloud tests by default; real remote smoke only when `BROWSER_USE_API_KEY` is present.
- TUI verification through `scripts/verify-terminal-ui.sh`.
- Bounded real-LLM smoke after deterministic tests pass, if provider credentials are available.

