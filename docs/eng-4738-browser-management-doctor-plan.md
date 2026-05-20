# ENG-4738 Browser Management + Doctor Plan

## Summary

Implement a Rust-owned browser control plane with a small explicit LLM surface:

- `browser`: CLI-style control/debug tool for connect/status/doctor/recovery/runtime management.
- `browser_script`: fresh Python execution tool for page interaction with browser helpers preimported.
- `view_image`: local image inspection tool documented as sequential and not parallel-safe.

Worktree: `/Users/greg/Documents/browser-use/experiments/llm-browser-eng-4738-browser-control`

Branch: `gregor/eng-4738-browser-management-doctor`

## Full Planning Context

This section is intentionally verbose. It captures the reasoning and product shape discussed before implementation so the engineer does not have to reconstruct the intent from a terse checklist.

### Original Problem

The current connection between the base harness and the browser is too fragile. Simple instructions like "go to my gmail.com" leave the agent unsure about:

- Which browser instance it is supposed to use.
- Which browser profile it is supposed to use.
- Whether it is using the user's already-logged-in real Chrome, a temp managed Chromium, or a remote cloud browser.
- What happened when CDP, the daemon, the browser process, the active target, or the session attachment dies.

The core issue is not just missing commands. The current premise is confusing because Python currently holds too much volatile browser state and browser-harness daemon state, while Rust owns the durable terminal/runtime state. This makes recovery and browser ownership hard to reason about. If Python is restarted, times out, or dies, the browser connection story becomes unclear.

The target direction is Rust-first:

- Rust owns browser connection state in the same spirit that Rust owns terminal command state.
- Python is a fresh scripting surface for page interaction, scraping, and browser-backed workflows.
- The LLM has explicit control over browser connection and recovery.
- Nothing reloads or relaunches the browser silently.
- If target IDs, session IDs, object IDs, or page IDs may change, the LLM sees that and chooses the next action.

### Most Important Product Rule

Do not hide browser lifecycle decisions from the LLM.

The LLM should get raw, explicit browser control as much as possible. The more the runtime tries to be clever with invisible reloads, auto-refreshes, tab switching, target replacement, and implicit daemon restarts, the worse the agent behaves. Browser automation performs best when the model can see:

- Current connection state.
- Current browser/profile/endpoint.
- Current target/session IDs.
- Whether the browser is external or runtime-owned.
- What recovery actions are safe.
- What exact command to run next.

### Current Implementation Facts

Current repo shape:

- Rust owns durable product state and the TUI.
- Python owns volatile browser namespace and browser-harness helper loading.
- The current `python` tool is persistent per session.
- Browser-harness is loaded from `/Users/greg/Developer/browser-harness/src` or `BROWSER_HARNESS_SRC`.
- Current browser-visible facts are recorded as events such as `browser.connected`, `browser.reconnected`, `browser.target_changed`, `browser.disconnected`, `browser.live_url`, `browser.page`, and `browser.state`.
- `crates/browser-use-browser` is currently a placeholder crate.
- `crates/browser-use-core/src/tools/mod.rs` currently exposes `python`, `view_image`, command/file tools, and helper-agent tools.
- `view_image` currently participates in parallel tool batches, but the desired behavior is sequential because visual context should not race with browser actions.
- TUI settings currently expose `Browser Use cloud`, `Local Chrome`, and `Headless Chromium`.
- Current TUI `Local Chrome` actually sets managed browser env and launches a visible temp Chrome through Python/browser-harness. That is a mismatch. "Local Chrome" should mean attaching to the already-open user browser after the user enables remote debugging.

Current Python/browser-harness behavior:

- Python starts a managed Chrome when `LLM_BROWSER_AUTO_CHROME=1` or headless mode is selected.
- Python starts Browser Use cloud through browser-harness when cloud mode is selected.
- Python patches browser-harness `cdp` so helper calls ensure the daemon first.
- Python auto-attaches screenshots and emits browser state events.
- Python namespaces persist by session ID, which makes reusable variables convenient but makes browser connection ownership confusing.
- The Python worker is supervised by Rust. On timeout Rust kills/restarts the worker and returns an error, but browser state may be tied to the worker/browser-harness layer.

### Browser-Harness Facts We Are Preserving

The current browser-harness repo at `/Users/greg/Developer/browser-harness` provides the reference behavior.

Important facts from `install.md`, `admin.py`, and `daemon.py`:

- Browser-harness architecture is currently:
  - Chrome or Browser Use cloud exposes CDP websocket.
  - `browser_harness.daemon` holds the websocket.
  - `browser_harness.run` and helpers talk to the daemon over IPC.
- `BU_CDP_WS` overrides local discovery for remote browsers.
- `BU_CDP_URL` points at a DevTools HTTP endpoint and resolves to websocket through `/json/version`.
- `BU_BROWSER_ID` plus `BROWSER_USE_API_KEY` lets Browser Use cloud browser be stopped on shutdown.
- Local real-profile flow:
  - User opens `chrome://inspect/#remote-debugging`.
  - User checks "Allow remote debugging for this browser instance".
  - Chrome 144+ may also show an "Allow remote debugging?" permission popup.
  - Harness then attaches to the user's already-open browser and inherits real logins, extensions, history, bookmarks, and cookies.
- Isolated automation browser flow:
  - Launch Chrome/Chromium with `--remote-debugging-port=<port>` and `--user-data-dir=<non-default path>`.
  - Set `BU_CDP_URL=http://127.0.0.1:<port>`.
  - Chrome 136+ ignores remote-debugging-port on the platform default user-data-dir.
  - Copying a default profile does not reliably preserve cookies because cookie encryption is bound to the original browser/profile context.
- Browser-harness local discovery scans known profile roots for `DevToolsActivePort`:
  - Google Chrome
  - Chrome Canary
  - Chromium
  - Microsoft Edge channels
  - Brave
  - Arc
  - Dia
  - Comet
  - common Linux/Windows equivalents
- Browser-harness also probes common ports like 9222/9223.
- Browser-harness `attach_first_page()` filters internal pages and creates `about:blank` if no real page exists.
- Browser-harness `attach_known_target()` reattaches an existing target after stale session errors.
- Browser-harness handles `"Session with given id not found"` by reattaching the known target before falling back to the first page.
- Browser-harness `run_doctor()` is read-only and reports platform, Chrome process, daemon, active page, local profile tooling, and API key state.
- Browser-harness `list_cloud_profiles()` returns `id`, `name`, `userId`, `cookieDomains`, and `lastUsedAt`.
- Browser-harness `start_remote_daemon()` creates a Browser Use cloud browser, resolves profile names, resolves `cdpUrl` to websocket, starts daemon, and returns browser details including `liveUrl`.
- Browser-harness `list_local_profiles()` shells out to `profile-use list --json`, but this terminal implementation now replaces that with native Rust filesystem discovery.
- Browser-harness `sync_local_profile()` exists, but local-to-cloud profile sync is not part of this ticket.

### Linear Project Context

ENG-4738 is "Browser Management + Doctor".

Related project tickets:

- ENG-4737 Runtime / SDK Contract
- ENG-4738 Browser Management + Doctor
- ENG-4739 Profiles / Cookies / Auth
- ENG-4740 Sessions / Tabs / Isolation
- ENG-4741 Observability + Terminal UX
- ENG-4742 Extraction Subagent
- ENG-4743 Network + Browser Fetch
- ENG-4744 Tools / Hooks / Skills

Original project direction:

- Local real Chrome management matters.
- Remote debugging permission/setup needs to be understandable.
- Doctor needs to explain what is wrong and what to do.
- Browser install/setup needs a clear path.
- Cloud browsers and cloud profiles matter.
- Profile/cookie/auth work matters, but should be split out after the browser management foundation.
- Future work should support tab locks, avoiding tab spam, network recorder, skills, auth/2FA, and customization.

Attached Linear document "Browser Use Interfaces" says the larger system should support:

- Agent runtime.
- Browser support.
- Cloud path.
- Auth/secrets.
- Navigation/tools.
- DOM and extraction.
- Files.
- LLM support.
- Customization.
- CLI.
- Monitoring.

Browser Use OSS already has useful concepts such as managed Chromium, headful/headless, `Browser.from_system_chrome()`, profile listing, existing CDP URL, Browser Use Cloud, third-party CDP, custom launch args, storage state, sensitive data, and domain credentials.

### Primary Architecture Decision

Browser ownership moves to Rust.

Rust should hold:

- CDP websocket connection.
- Browser endpoint information.
- Browser owner.
- Current target ID.
- Current session ID.
- Connection generation.
- Managed browser child process if Rust launched it.
- Remote cloud browser ID if Rust created it.
- Live URL if remote cloud provides it.
- Browser/profile/candidate metadata.
- Browser event/log ring buffer.
- Safe recovery/action availability.

Python should not be the persisted browser owner.

Python should be:

- A fresh process per `browser_script` call.
- A high-power scripting surface for page interaction.
- Preloaded with helpers so the LLM can do useful browser work with little boilerplate.
- Transparent enough that the LLM can read/use helper logic and raw CDP instead of opaque wrappers.

### Why Not Make Python A Wrapper Around Rust Helpers

Do not make Python/Node scripting tools opaque wrappers around Rust browser implementations.

The LLM needs to understand what went wrong. If helpers hide everything behind Rust calls with no visible logic, debugging becomes worse. The browser-harness style is useful because the model can read the helper API, use raw CDP, and reason about failure modes directly.

The compromise:

- Rust owns connection and lifecycle.
- Python gets an explicit, documented bridge to send raw CDP commands through Rust.
- Python helpers are visible/simple and mostly compose raw CDP.
- Browser connection/debug/recovery lives only in the `browser` tool.
- Page interaction/scraping lives only in `browser_script`.

### No Duplicate Functionality Rule

Avoid duplicating functionality in both `browser` and `browser_script`.

`browser` owns runtime/control/debug:

- Connect.
- Start.
- Stop.
- Status.
- Doctor.
- Recovery.
- Profiles.
- Runtime logs.
- Ownership.
- Stale cleanup.

`browser_script` owns page interaction/data plane:

- Navigate.
- New tab.
- JavaScript.
- Raw CDP commands against the selected target/session.
- Click/type/scroll.
- Screenshots.
- Extraction.
- Upload/download workflows.
- Artifacts/final answers.

`view_image` only inspects local image files.

### LLM-Visible Tool Footprint

The LLM should see:

1. `browser`
2. `browser_script`
3. `view_image`
4. `done`
5. The existing non-browser tools such as file/command/plan/helper-agent tools.

Browser-related footprint:

- `browser` has one input: a raw CLI-like command string.
- `browser_script` has one input: Python code.
- `view_image` has path/detail and is sequential.

The old browser-heavy `python` tool should be replaced or relegated to compatibility. The intended interface is not "Python owns browser". It is "browser controls runtime; browser_script interacts with page".

### `browser` Tool Description Requirements

The `browser` CLI reference must be part of the tool description itself. It can also live in a prompt/doc file and be included with `include_str!`, but it must be visible to the LLM as tool documentation.

The description should read like a small README, not like a tiny enum schema.

It must explain:

- Mental model.
- Local vs managed vs remote.
- Real profile rule.
- Remote start means start and connect.
- Doctor is read-only.
- Recovery commands do not reload pages silently.
- Which commands are safe for external browsers.
- Which commands only work for Rust-owned browsers.
- The exact CLI command list.
- Example flows.
- What to do when local connection is blocked by Chrome remote-debugging approval.
- What to do when multiple local candidates are found.
- What to do when Browser Use API key is missing.

### `browser` Command Shape

The `browser` tool accepts one raw command string, for example:

```text
browser status --json
browser connect local
browser local list --json
browser local setup
browser connect managed --headed
browser remote start --profile-name Work
browser recover reconnect-websocket
```

The tool implementation parses this command. The LLM sees it like a CLI. We do not need a giant JSON enum of actions.

Approved commands:

- `help`
- `status --json`
- `doctor`
- `doctor --json`
- `connect local`
- `connect local --candidate <id>`
- `connect managed [--headless|--headed] [--profile temp|<path>] [--arg <chrome-arg>...]`
- `connect remote-cdp --url <http-url>`
- `connect remote-cdp --ws <ws-url>`
- `local list --json`
- `local setup`
- `local profiles --json`
- `local profiles inspect <profile-name> --domains-only`
- `remote start [--profile-id <uuid> | --profile-name <name>] [--timeout <minutes>] [--proxy-country <iso2|none>]`
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

Removed/deferred commands:

- `doctor --fix-safe`: removed. Doctor is read-only. If a fix exists, doctor prints the exact command.
- `remote sync-local-profile`: deferred to ENG-4739.
- Managed `--profile-copy`: deferred until auth limitations can be communicated honestly.
- Tab lock/cleanup/network/auth commands: deferred.

### Local Browser Behavior

`browser connect local` should attach to an already-running Chromium-family browser that has remote debugging enabled.

It should not require the LLM to know whether the browser is Chrome, Canary, Edge, Brave, Arc, Dia, Comet, or Chromium. The agent should not guess `--browser chrome` or similar.

Behavior:

- Auto-detect usable running Chromium-based candidates.
- If exactly one candidate exists, connect.
- If multiple candidates exist, return candidate IDs and ask the user which browser to use.
- If no candidate exists, report either browser not running or remote debugging not enabled.
- `browser local list --json` lists candidates.
- `browser connect local --candidate <id>` connects a user-chosen candidate.
- `browser local setup` opens `chrome://inspect/#remote-debugging` and tells the user to allow remote debugging.

Known browser families to support initially:

- Google Chrome
- Chrome Canary
- Chromium
- Microsoft Edge
- Brave
- Arc
- Dia
- Comet

Configured/custom forks can come later.

### Real Profile Rule

For a real logged-in browser profile, attach to the already-open browser. Do not launch Google Chrome with the real default profile and `--remote-debugging-port`.

Reason:

- Chrome 136+ ignores remote debugging with the platform default user-data-dir.
- Chrome has profile locks.
- Copying default profiles does not reliably preserve cookies.
- Cookie encryption can be bound to the original profile/browser context.

Managed browser mode is for temp or explicit non-default profiles.

### Managed Browser Behavior

`browser connect managed` means Rust starts and owns a browser process.

Allowed:

- Temp profile.
- Explicit non-default user-data-dir.
- Headless or headed.
- Custom launch args only when needed.

Safety:

- Rust can restart/stop this browser because Rust owns it.
- Managed browser recovery may change target/session IDs and must report that.
- Managed browser should not pretend to be the user's real logged-in Chrome.

### Remote Browser Behavior

Remote modes:

- `browser connect remote-cdp --url <http-url>` attaches to an externally provided DevTools HTTP endpoint.
- `browser connect remote-cdp --ws <ws-url>` attaches to an externally provided CDP websocket.
- `browser remote start ...` creates a Browser Use cloud browser and connects to it.

Important rule:

`remote start` means start and connect.

The agent should not have to manually copy `cdpUrl` out and pass it into another command. The command should create the browser, resolve/connect CDP, store ownership, return `liveUrl`, and report status.

### Profiles And Cookies

Local profile commands are allowed, but raw cookies are not dumped by default.

`browser local profiles --json`:

- Uses native Rust filesystem discovery.
- Returns detected profile names/paths/browser names.
- Does not require `profile-use` or any other external CLI.

`browser local profiles inspect <profile-id-or-name> --domains-only`:

- Copies the selected profile into a temporary user-data-dir, launches that temporary copy with CDP, and reads cookies through `Storage.getCookies`.
- Shows domains/counts/expiry summaries only.
- Does not expose raw cookie values by default.

`browser remote profiles --json`:

- Uses Browser Use API.
- Returns cloud profile ID/name/domain summary/last used.
- Does not expose raw cookie values.

Local-to-cloud sync is deferred.

### Status Output

`browser status --json` should include clear, model-useful state:

- `mode`: `none`, `local`, `managed`, `remote-cdp`, `remote-cloud`
- `connection`: `connected`, `disconnected`, `not-configured`, `blocked`
- `reason`: optional clear reason
- `next_step`: exact suggested next command when useful
- `owner`: `external` or `rust`
- `browser`: browser name if known
- `profile`: profile name/path if known
- `endpoint`: `devtools-active-port`, `cdp-url`, `cdp-ws`, `browser-use-cloud`
- `page`: `target_id`, `session_id`, `url`, `title`
- `safety`: `can_restart_browser`, `can_close_browser`, `can_stop_remote`
- `connection_generation`: increments when websocket/session/target attachment changes
- `live_url`: when remote browser provides it

`connection_generation` is for debugging stale context. It does not need to be prominent in the UI, but it should exist in JSON/logs.

### Doctor Behavior

Doctor is read-only.

`browser doctor` and `browser doctor --json` should check:

- Runtime state files/sockets.
- Browser ownership.
- Managed browser PID if owned.
- Remote cloud ownership if owned.
- Local browser processes/candidates.
- Known profile roots.
- DevToolsActivePort existence/staleness.
- CDP HTTP endpoint reachability.
- CDP websocket handshake.
- Current target exists and is a real page.
- Session attachment health.
- Browser Use API key presence.
- Browser Use API reachability when needed.
- Rust local profile discovery.
- Cloud profile list access when needed.
- Stale runtime files that can be safely cleaned.

Doctor does not fix anything automatically. It returns exact next commands such as:

- `browser local setup`
- `browser connect local`
- `browser connect local --candidate <id>`
- `browser recover reconnect-websocket`
- `browser recover reattach-same-target`
- `browser runtime cleanup-stale`

### Recovery Model

Recovery must be explicit.

No silent browser reloads.
No silent daemon/browser restarts.
No silent target switching.
No silent tab creation except where the LLM explicitly asks for it through page helpers.

Approved recovery commands:

`recover reconnect-websocket`

- Problem: CDP websocket dropped, browser/tab may still be alive.
- How we know: read EOF, websocket close frame, write failure, heartbeat/probe timeout, TCP refusal, or HTTP endpoint still alive while websocket is gone.
- Action: reconnect to the same endpoint.
- Does not reload the page.
- Does not change target if the target still exists.

`recover reattach-same-target`

- Problem: current CDP session/attachment is stale, often visible as `"Session with given id not found"`.
- This does not always mean the target is gone.
- Target may still exist while the session ID is invalid.
- Action: call `Target.getTargetInfo` or list targets, confirm old `target_id` still exists, and attach again.
- If target is gone, report that clearly and list available targets; do not silently switch.

`recover restart-runtime`

- Problem: Rust runtime connection holder is wedged, but browser should not be touched.
- Action: reset Rust connection state and reconnect/re-attach as explicitly requested.
- Does not kill external Chrome.
- Does not stop remote browser.

`recover restart-owned-browser`

- Problem: Rust-owned managed browser is dead/wedged.
- Only works for managed browsers Rust launched.
- Not allowed for external user Chrome or externally supplied CDP.

`recover stop-owned-remote`

- Problem: Rust-created Browser Use cloud browser should be stopped.
- Only works when Rust owns the cloud browser ID.
- Does not affect external CDP URLs or user Chrome.

### Runtime Debug Commands

`runtime logs`

- Shows recent browser runtime/control-plane logs.
- Solves "why did connect fail?" without making the LLM guess.

`runtime ownership --json`

- Shows what the runtime owns:
  - Runtime ID.
  - State paths.
  - Socket paths.
  - Current endpoint.
  - Current owner.
  - Managed PID.
  - Remote browser ID.
  - Current target/session.
  - Safe kill/stop/restart booleans.

This answers "what can be safely killed or stopped?"

`runtime cleanup-stale`

- Removes stale runtime files/sockets only when proven not live.
- Does not kill Chrome.
- Does not stop cloud browsers.

### `browser_script` Tool

`browser_script` runs Python for browser/page work.

It should be explained in the tool description in plain language, not as "browser-harness-style" without context.

Core rules:

- Fresh Python process per tool call.
- Browser/CDP state persists in Rust.
- Python variables do not persist.
- Helpers are preimported.
- Raw CDP is available through `cdp(...)`.
- Page JavaScript is available through `js(...)`.
- Screenshots and artifacts attach to the task.
- Use this for browser navigation, browser inspection, clicks, typing, screenshots, downloads, uploads, network inspection, extraction, and browser-backed verification.
- Do not import Playwright, Selenium, or Pyppeteer.
- Use screenshots as labeled temporal checkpoints.
- Prefer coordinate clicks for visible UI when appropriate.
- Use raw CDP/JS when coordinates are the wrong tool.

Preimported helpers:

- `cdp(method, session_id=None, **params)`
- `cdp_batch(calls)`
- `js(expression, returnByValue=True)`
- `new_tab(url="about:blank")`
- `goto_url(url)`
- `page_info()`
- `capture_screenshot(...)`
- `screenshot(label="screenshot", full=False)`
- `screenshot_clip(label, x, y, width, height)`
- `click_at_xy(x, y)`
- `fill_input(...)`
- `type_text(text)`
- `press_key(key)`
- `scroll(...)`
- `wait_for_load(timeout=10)`
- `wait_for_element(...)`
- `wait_for_network_idle(timeout=10)`
- `current_tab()`
- `list_tabs()`
- `switch_tab(target_id)`
- `ensure_real_tab()`
- `upload_file(...)`
- `drain_events()`
- `http_get(...)`
- `copy_artifact(path, kind="file")`
- `emit_image(path, label=None)`
- `set_final_answer(data, artifact_name=None)`
- `audit_artifact(...)`
- `load_agent_helpers()`
- `agent_workspace()`

Helpers intentionally not included in `browser_script`:

- `browser_status`
- `browser_doctor`
- `browser_connect`
- `browser_recover`
- profile management helpers
- remote browser management helpers

Those belong to `browser`.

### Tab Spam Prevention

For this ticket:

- `goto_url(url)` navigates the current controlled tab.
- `new_tab(url)` creates a tab only when explicitly called.
- `ensure_real_tab()` reuses the current/known real tab where possible.
- Runtime tracks active target and tabs created by this session.

Deferred:

- Tab locks.
- Subagent tab isolation.
- Auto-close created tabs policy.
- Same-tab/new-tab policy UI.

### `view_image`

`view_image` is a pure Rust local image inspection tool.

It must be documented as:

- Sequential.
- Not parallel-safe.
- Useful for inspecting screenshots/artifacts already saved to disk.
- Not a browser screenshot tool.

Browser screenshots should be created through `browser_script`, then inspected via model image attachments or `view_image` when needed.

### Where Functions Live

Rust/browser crate:

- CDP client.
- Websocket state.
- Local discovery.
- Managed browser process.
- Browser Use cloud API calls.
- Status/doctor/recovery.
- Runtime logs/ownership/cleanup.
- Browser control command parser.

Core tool registry:

- `browser` tool definition and dispatch.
- `browser_script` tool definition and dispatch.
- `view_image` sequential documentation/parallelism rule.

Python/browser script support:

- Fresh process entry point.
- Helper prelude.
- Artifact/final answer helpers.
- Raw CDP bridge back to Rust.
- No durable browser ownership.

TUI:

- Browser choice labels/settings.
- Runtime options.
- Browser status rendering.
- Browser overlay/help text.
- Local Chrome behavior fixed to attach instead of launching managed Chrome.

### High-Level Summary For Approval

We will implement:

1. One `browser` tool with a CLI-like `cmd`.
   - This is the control plane.
   - It manages local attach, managed browser launch, remote CDP attach, Browser Use cloud start/connect, status, doctor, recovery, profiles, runtime logs, ownership, and stale cleanup.
   - It does not interact with web pages.

2. One `browser_script` tool with Python code.
   - This is the page/data plane.
   - It has preimported helpers and raw CDP access.
   - It runs in a fresh Python process each call.
   - It does navigation, inspection, screenshots, clicks, typing, extraction, artifacts, and final answers.
   - It does not manage browser runtime connection.

3. Rust owns browser connection state.
   - Rust holds websocket/session/target/generation/ownership.
   - Python no longer owns the durable browser connection.
   - Recovery is explicit and visible to the LLM.

4. Local Chrome means real local attach.
   - Attach to an already-open browser after the user enables remote debugging.
   - Do not launch the user's real default profile with remote-debugging args.
   - Managed browser is a separate mode with temp or non-default profile.

5. Remote browser start means start and connect.
   - Browser Use cloud `remote start` creates the cloud browser, connects CDP, stores ownership, and returns live URL.
   - The LLM should not manually shuttle CDP URLs between commands.

6. Doctor is read-only.
   - It explains what is wrong and gives exact next commands.
   - It never mutates browser/runtime state by itself.

7. Recovery commands are explicit.
   - Reconnect websocket.
   - Reattach same target.
   - Restart runtime holder.
   - Restart only Rust-owned managed browser.
   - Stop only Rust-owned remote browser.

8. `view_image` becomes sequential.
   - The model must know it is not parallel-safe.
   - It remains for inspecting saved local image artifacts.

9. Deferred work stays out of ENG-4738.
   - No profile sync.
   - No copying real profiles.
   - No tab locks.
   - No network recorder.
   - No auth/TOTP/secrets tooling.
   - No custom browser-family config UI.

## Checklist

- [x] Commit this plan before implementation.
- [x] Add Rust browser runtime in `crates/browser-use-browser`.
- [x] Add `browser` CLI-style tool and README-like tool description.
- [x] Add `browser_script` fresh Python runner and helper preload.
- [x] Move browser connection ownership out of persistent Python state.
- [x] Support local attach, managed browser, remote CDP, and Browser Use cloud start/connect.
- [x] Add status, doctor, recovery, runtime logs, ownership, and stale cleanup commands.
- [x] Mark `view_image` sequential/not parallel-safe.
- [x] Update TUI/settings/state to reflect local attach vs managed launch.
- [x] Add unit/integration tests for parser, status, doctor, recovery, and scripting.
  - Added contract tests for the LLM-visible `browser`, `browser_script`, and sequential `view_image` descriptions.
  - Added browser runtime tests for command parsing, status shape, doctor output, explicit recovery failure, fresh `browser_script`, and an ignored real managed-browser smoke.
- [x] Run `cargo fmt --check`.
  - Passed.
- [x] Run `cargo test`.
  - Passed.
- [x] Run `uv run --with pytest python -m pytest -q`.
  - Passed: 19 tests.
- [x] Run `scripts/verify-terminal-ui.sh` for TUI coverage.
  - Passed. Artifacts inspected under `/tmp/but-design-loop/`.
- [x] Run bounded real-LLM smoke test if credentials are available; otherwise record blocker.
  - Passed with Codex `gpt-5.4-mini`, state dir `/tmp/but-real-llm-smoke`, session `2208a8d6ebb7`.
  - The model called `browser status --json`, `browser connect managed --headless`, `browser_script`, created screenshot artifact `llm_smoke`, read `document.title`, and finished with `title=LLM Browser Smoke`.

## Native Local Profile Follow-Up

Problem found after the first implementation:

- The LLM-visible flow said local profiles were unavailable unless `profile-use` was installed.
- That is the wrong dependency boundary. The terminal must not rely on an external Browser Use CLI for its own browser-profile inventory.
- The old `profile-use` repo only provided the pieces we need as filesystem scanning, Local State parsing, temporary profile copy, browser launch, and CDP cookie-domain summary.

Implemented now:

- [x] Replaced `profile-use list --json` with native Rust filesystem discovery.
- [x] Added Chromium-family profile roots for Chrome, Chrome Canary, Chromium, Edge, Brave, Arc, Dia, Comet, Opera, Vivaldi, Yandex, Iridium, Helium, Sidekick, Thorium, SigmaOS, Wavebox, Ghost Browser, and Blisk where known.
- [x] Read profile display names from Chromium `Local State` `profile.info_cache`.
- [x] Return stable profile ids such as `google-chrome:Default` and include browser/profile paths in JSON output.
- [x] Replaced local profile inspect with native Rust temporary-profile copy plus CDP `Storage.getCookies`.
- [x] Return domain/count/session/expiry summaries only.
- [x] Never return raw cookie names or values by default.
- [x] Updated LLM-visible descriptions to say local profile discovery is Rust built-in and to explain quoted profile ids.
- [x] Updated doctor to check Rust local profile discovery instead of `profile-use` installation.
- [x] Added tests for native profile command dispatch, Local State names, ambiguous profile resolution, temporary profile copy skips, and cookie summary redaction.
- [x] Verified with `cargo fmt --check`, `cargo test`, `uv run --with pytest python -m pytest -q`, and the ignored managed-browser smoke.

Still deferred:

- Custom browser-family config UI.
- Local-to-cloud profile sync.
- Raw cookie export.
- Launching the user's real default profile with remote-debugging flags.

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
