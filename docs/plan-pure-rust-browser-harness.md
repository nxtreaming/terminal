# Spec: pure Rust browser harness

## Goal

Build a pure Rust browser runtime aligned with browser-harness:

- one websocket to Chrome
- raw CDP first
- screenshots and browser events as first-class observations
- coordinate/compositor actions over selector frameworks
- agent-editable workspace and domain skills
- no Playwright, Selenium, Python worker, or Python daemon

This is effectively `browser-harness-rs`: same ideology, Rust-native runtime.

## Core architecture

```text
Agent
  -> Rust browser tools
  -> Rust CDP session manager
  -> Chrome / Browser Use Cloud
```

Browser tools are registered eagerly, but the browser process/session is initialized lazily.

## Launch policy

Do not launch Chrome when the agent starts.

Modes:

- `lazy`: default. Launch/connect on the first browser tool call.
- `warm`: browser tools are still lazy, but Rust may proactively start the browser in the background for browser-heavy evals.
- `disabled`: do not expose browser tools.

Example:

```text
LLM_BROWSER_RUNTIME=rust
LLM_BROWSER_BROWSER_LAUNCH=lazy
```

## Browser tools

- `browser_cdp`: raw CDP call.
- `browser_cdp_batch`: multiple CDP calls plus waits in one tool call.
- `browser_eval`: thin `Runtime.evaluate`.
- `browser_screenshot`: explicit screenshot tool with image attachment.
- `browser_targets`: list, switch, create, and close tabs.
- `browser_reset`: reconnect or reset stale browser state.
- `browser_http`: fast non-browser HTTP for bulk/static extraction.

Raw CDP must remain the escape hatch and normal control surface. Do not add wrappers for basic navigation, evaluation, or mouse dispatch unless the wrapper adds host-side value.

## Screenshot rule

Screenshots are browser observations, not paths the model has to reopen.

Every `Page.captureScreenshot` result must be intercepted by Rust:

- decode the image bytes
- write a local artifact
- attach the image directly to the browser tool response
- keep the CDP result compact

Do not require `view_image`/`read_image` for browser screenshots. Those tools are for existing local files, not the primary browser observation loop.

## Remote browser file rule

Remote browser files live on the remote browser machine.

The model must not assume:

```text
browser file path == local machine file path
```

Any browser-produced file that matters must be transferred back through the browser runtime and saved locally as an artifact.

This applies to:

- screenshots
- PDFs
- downloads
- canvas/image exports
- files generated inside the page

Uploads are the reverse bridge: local files must be made available to the remote browser through the runtime.

## CDP session manager

Rust owns:

- CDP websocket connection
- request ids and pending response map
- event stream
- active target id
- active session id
- target attach/detach
- stale session recovery
- browser-owned artifacts

On attach, enable:

- `Page`
- `Runtime`
- `DOM`
- `Network`
- `Target`
- `Browser`

Filter internal targets such as `chrome://`, `devtools://`, extension pages, and omnibox popups.

## Browser events

Rust emits durable browser events and compact model-visible summaries.

Important events:

- connected/disconnected
- target changed
- tab opened/closed
- dialog opened
- download started/completed
- navigation
- screenshot captured
- stale session recovered
- viewport changed

The model-visible summary should stay tiny and should not dump raw CDP event payloads.

## Agent workspace

Keep the browser-harness learning loop:

```text
.browser-use/agent-workspace/
  agent_helpers.js
  domain-skills/
```

Use `agent_helpers.js` for small reusable browser-side helpers executed through Chrome via `Runtime.evaluate`.

Domain skills remain Markdown playbooks and should stay compatible with the browser-harness style.

## Managed browser modes

Support:

- Rust-owned local Chrome/Chromium for evals.
- External CDP via `BU_CDP_URL` or `BU_CDP_WS`.
- Browser Use Cloud via API/CDP URL.

Rust must only shut down browsers it started or cloud browsers it created.

## Main phases

### 1. Rust CDP core

Create a Rust browser crate that connects to Chrome, sends raw CDP, receives events, and attaches to a page target.

### 2. Lazy managed browser

Implement lazy browser launch/connect. Tools are available immediately; Chrome starts only when needed.

### 3. Target/session manager

Track active tab/session, recover stale targets, and filter internal targets.

### 4. Browser tools

Expose Rust-native `browser_*` tools and route browser work away from Python.

### 5. Screenshot and remote-file bridge

Auto-attach screenshots and make downloads/PDFs/generated files local artifacts.

### 6. Agent workspace

Support `.browser-use/agent-workspace/agent_helpers.js` and domain skill lookup.

### 7. Downloads, uploads, dialogs, cloud

Port file upload, downloads, PDFs, dialogs, and Browser Use Cloud start/stop into Rust.

### 8. Benchmark and flip default

Run `real_v14_short` and `real_v8`, compare against the current Python worker, then make the Rust runtime default once it reaches parity or wins.

## Do not build

- Playwright-style abstraction.
- Selector-first framework.
- Python compatibility runtime.
- Node/Bun runtime.
- Huge helper library.
- Hidden manager layer.

## Migration path

Build behind:

```text
LLM_BROWSER_RUNTIME=rust
```

Keep the current Python path as a legacy fallback until the Rust runtime beats or matches it on benchmarks. Then delete or demote Python.
