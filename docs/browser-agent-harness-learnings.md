# Browser Agent Harness Learnings and Ideas

This document is the high-level design memo. The implementation plan is in `docs/browser-agent-harness-plan.md`, and the MVP-vs-later execution split is in `docs/implementation-roadmap.md`.

## Core Thesis

The thing to build is opencode plus the browser harness in one product:

- opencode's session/event/background-agent architecture
- harnesless's raw CDP freedom and editable helper layer
- a browser-specific tool surface that solves visual observation without extra screenshot-reading tool calls

The browser harness should not try to be a clever web automation framework. It should be a reliable operating substrate for an LLM that already knows how to write code, use CDP, call JavaScript, inspect pages, run shell commands, and invent helpers.

The right abstraction is:

```text
robust substrate + tiny powerful tools + editable helper layer + durable sessions
```

Not:

```text
large semantic browser API + many brittle assumptions + fixed workflows
```

## The Bitter Lesson Applied to Browser Agents

The Browser Use article's lesson maps cleanly onto this project:

- Do not hide the browser behind only high-level actions.
- Do not assume a fixed interaction path for all sites.
- Do not make the framework smarter than the model at the task layer.
- Do make the low-level runtime reliable.
- Do let the model write, edit, and reload helper code during the task.
- Do expose the real browser state through multiple channels.

The harness should feel closer to a browser-aware Python REPL than to a big browser automation SDK.

## Screenshot Read Problem

The current coding-harness failure mode is:

```text
tool call: interact with browser
model continuation
tool call: capture screenshot
model continuation
tool call: view/read screenshot
model continuation
```

The target harness should collapse this to:

```text
tool call: interact with browser, capture multiple step screenshots, return them as an ordered image timeline
model continuation: sees the temporal sequence immediately
```

This is not "zero model calls after a tool." Tool calling still requires the model to continue after the application executes the tool. The win is removing the separate screenshot capture/read tool round trip.

The browser/Python tool should let the model do:

```python
new_tab(url)
wait_for_load()
screenshot("loaded_homepage", attach=True)
click_at(x, y)
screenshot("after_click", attach=True)
wait_for_network_idle()
screenshot("after_network_idle", attach=True)
```

`attach=True` means the screenshot becomes part of the tool result as model-visible image content, not just a file path. It is also saved as an artifact for the TUI and replay.

The temporal component is not optional. The model often needs to verify that each action really happened: page loaded, click landed, modal opened, list changed, network settled. The harness should return multiple images from one tool call in order, each with a label, timestamp, URL/title, and artifact path. The TUI should show those frames as they are emitted while the tool is still running.

The public OpenAI Responses API supports function tool outputs that are text, image, or file content. For Codex subscription auth we still need a live smoke test against the ChatGPT Codex backend. If it does not accept images directly inside function-call output, the harness should inject the screenshot as an immediate synthetic image input item after the tool result. The product behavior remains the same: no separate screenshot-reader tool call.

Do not attach whole DOM trees by default. If the model needs page structure, it can run raw CDP or JavaScript and return a small purpose-built result: visible text, links, forms, a selected accessibility slice, or a site-specific extraction.

## Simple Tool Concurrency

Do not overbuild scheduling. The runtime should copy the good coding-harness ability to run independent tools in parallel, but keep browser-state coordination simple.

The MVP scheduler should:

- run independent reads/searches/status checks in parallel
- serialize one persistent Python/browser action per session
- serialize CDP mutations per browser target/profile
- serialize file writes per file

Do not expose barriers or lock keys to the model in v1. If the model wants ordering, it can put ordered browser actions inside one Python call.

## Event-Driven Long Tasks

Long tasks and subagents should not be implemented as blocking wait loops.

The core primitive should be:

```text
start session/task
subscribe to events
receive tool/text/screenshot/status events
cancel or steer if needed
read final result when done
```

A convenience `await_done()` is fine for scripts, but the runtime and TUI should be event-driven. This is what makes cancellation, progress display, child-session inspection, and live screenshot timelines natural.

## First-Principles Mental Model

A web task is a control problem over a stateful remote system.

State is distributed across:

- Chrome targets
- DOM
- JavaScript runtime
- visual pixels
- network requests
- cookies and storage
- downloads
- OS dialogs
- page timers
- remote site state
- the LLM's own context

Failures happen when the harness narrows this state too much. If the harness only exposes DOM selectors, visual/canvas/iframe/native cases fail. If it only exposes screenshots, structured extraction is slow. If it only exposes high-level "click button" actions, the model cannot recover when that action lies or does nothing.

The right answer is not one perfect observation. It is composable observation:

- screenshot
- DOM
- accessibility tree
- network
- console
- target/tabs
- storage
- downloads
- raw CDP
- shell fallback

## What to Build Robustly

Some components must be boring and dependable:

- CDP connection ownership
- browser process/profile lifecycle
- target selection and tab tracking
- IPC between tools and daemon
- screenshots/artifacts
- session history
- tool execution and cancellation
- output truncation/spillover
- compaction
- Codex auth and refresh
- provider streaming parser
- TUI event replay

These should not be agent-hacked every task. They are the floor.

## What to Keep Dynamic

Most browser intelligence should be dynamic:

- helpers for a site
- extraction scripts
- cookie-consent recipes
- navigation heuristics
- table parsing
- visual inspection strategy
- direct API calls discovered from network
- AppleScript/OS workarounds
- PDF processing recipes
- task-specific scratch code

These should live in editable workspace files and session scratch directories.

## Lessons From Harnesless

What to copy:

- One websocket to Chrome.
- Raw `cdp(method, params)` as a first-class primitive.
- Tiny CLI/runtime surface.
- Helpers preloaded into a Python command environment.
- `agent-workspace/agent_helpers.py` as the self-improvement point.
- The model can inspect and edit the helper functions it was shown in the first examples.
- The first examples should include pure raw CDP calls, not only wrappers.
- Coordinate clicks as a reliable default.
- Re-screenshot after meaningful actions.
- Daemon owns CDP and survives across tool calls.
- Stale-session recovery and target filtering matter.

What to improve:

- Turn it from a helper CLI into a full agent runtime.
- Add sessions, event storage, compaction, provider auth, TUI, evals.
- Reduce screenshot round trips by letting one Python tool call run multi-step browser code and return screenshot artifacts.

## Lessons From BU

What to copy:

- The `Agent` primitive shape is good.
- Typed events are good.
- `done` as a tool is useful.
- Hooks around tool execution are useful.
- Dependency overrides make tools testable.
- Token-aware truncation and artifact spillover are necessary.
- Compaction needs first-class support.

What not to copy:

- The BU-specific service layer and browser-use assumptions.
- Special subagent handling.
- Large task-specific tool catalogs as the main browser interface.

The new package should look structurally familiar to BU, but public concepts should be `agent`, `session`, `tool`, `browser`, and `workspace`, not BU.

## Lessons From opencode

The most important opencode idea is subagents as normal sessions.

The `task` tool:

- creates a real session
- records `parentID`
- prompts that session
- returns a task/session id
- lets the caller resume it

That is exactly what this harness should do. A background browser worker should not be a different internal species. It should be a session in the same store with the same event stream and artifact model.

Other useful ideas:

- agent registry/modes can exist, but modes should be lightweight
- compaction templates should preserve goal, constraints, progress, decisions, next steps, and relevant files/artifacts
- huge tool outputs should be saved to files with a compact preview
- file tools should be copied closely: `read`, `glob`, `grep`, `edit`, `write`, and `apply_patch`
- edits should return diffs, preserve line endings/BOM, lock per file, and publish file-change events

Avoid:

- importing all coding-agent modes and prompts
- making git/review/codebase norms central to browser tasks

## Lessons From pi-mono

pi-mono has a clean low-level agent loop:

- transform context at the model boundary
- stream events
- run tools in parallel when safe
- keep sequential tools sequential when necessary
- expose steering and follow-up queues
- make hooks explicit
- keep provider/runtime failures in the stream contract

This is a good model for our agent runtime.

Its Codex Responses provider is also directly useful:

- `store: false`
- `stream: true`
- `prompt_cache_key: sessionId`
- `parallel_tool_calls: true`
- `include: ["reasoning.encrypted_content"]`
- WebSocket/SSE handling
- retry handling
- model/provider normalization

## Lessons From Codex

Codex is heavy, but the substrate engineering is strong.

Useful pieces:

- session config separate from turn context
- one active turn per session
- mailbox/pending input/interrupt model
- event protocol for TUI replay
- tool router separate from tool runtime
- unified exec with process ids, PTY, stdin polling, output buffering, cancellation
- streaming apply-patch progress events
- compaction as a normal evented task
- PKCE localhost auth server with fallback port and state validation
- token storage with file/keyring modes

Things to avoid:

- permission/sandbox machinery as the default center of the product
- coding-heavy system prompts
- git workflow assumptions
- broad plugin/skill discovery complexity in v1

For a browser agent, the equivalent of Codex unified exec is a persistent Python browser runtime plus a shell runtime. Those should be excellent.

## Lessons From Hermes and OpenClaw

Hermes adds an important auth warning: do not casually share Codex CLI refresh tokens.

Refresh tokens can rotate or be single-use. If this harness, Codex CLI, and VS Code all refresh the same token, one can invalidate the others. Therefore:

- use a harness-owned auth store
- device-code login is a good first flow
- importing `~/.codex/auth.json` should be explicit and one-time
- do not silently re-seed from Codex CLI auth every run
- refresh under a file lock
- classify refresh failures clearly

OpenClaw adds useful operational ideas:

- non-prompting Codex CLI credential reads are fine for status detection
- auth profiles need health/staleness tracking
- do not let `OPENAI_API_KEY` or `CODEX_API_KEY` accidentally override a subscription-backed Codex run
- strip unsupported payload fields for Codex-compatible endpoints
- dynamic tool calls must have bounded timeouts and explicit failure results

Hermes' TUI gateway adds a useful UI/runtime pattern:

- keep the TUI as a transport over events, not the owner of agent logic
- use JSON-RPC over stdio/WebSocket boundaries
- route slow handlers to a worker pool so interrupts/approvals/cancel messages do not starve
- use best-effort sidecar publishing for dashboards without blocking the main UI
- write crash logs and surface short error lines in the UI

For this browser harness, that means the TUI should subscribe to session/tool/browser events and stay responsive while long browser tools or child sessions run.

## Main Design Decisions

### Build Rust-First, With One Python Island

The final rewrite decision is not "build the product in Python." Python remains the right place for the browser-connected worker because browser-harness, CDP helpers, scraping libraries, PDF/table/image tools, and model-editable helper code all fit naturally there.

Rust should own the durable product runtime:

- TUI and CLI
- SQLite state
- event log
- agent loop
- provider adapters
- cancellation/resume
- sub-agent graph

This keeps the product simple while preserving the browser-harness property that the code touching CDP also understands connection/session/target identity.

### Keep Tools Few

Preferred tool surface:

- `python`: persistent browser-aware Python runtime
- `shell`: OS escape hatch and long-running processes
- `files`: read/search/edit helper files and artifacts
- `session`: create/resume/wait/abort/list sessions
- `done`: final answer

This is enough for broad browser autonomy without a huge tool schema.

### Make CDP the Assembly Language

Every helper should reduce friction, not remove access.

The model should always be able to call:

```python
cdp("Page.navigate", {"url": url})
cdp("Runtime.evaluate", {"expression": js, "awaitPromise": True})
```

Helpers like `click_at`, `wait_for_load`, and `observe` should be short and editable.

### Make Python the Batch Action Surface

To reduce latency, the model should be able to do this in one tool call:

```python
new_tab("https://example.com")
wait_for_load()
screenshot("loaded", attach=True)
print(js("document.title"))
```

Or this:

```python
for page in range(1, 20):
    screenshot(f"page_{page}_before_extract", attach=True)
    rows = js("extract rows from current page")
    save_jsonl("rows.jsonl", rows)
    if not click_next_page():
        break
screenshot("final", attach=True)
```

That is the core speed improvement over coding harnesses.

### Keep Raw CDP Visible

The starter docs shown to the model should include pure CDP, for example:

```python
cdp("Page.navigate", {"url": "https://example.com"})
cdp("Runtime.evaluate", {"expression": "document.title", "returnByValue": True})
cdp("Input.dispatchKeyEvent", {"type": "keyDown", "windowsVirtualKeyCode": 13})
```

Helpers are convenience functions, not the interface boundary. The model should be able to open the helper source, edit it, reload it, or bypass it.

### Sessions, Not Subagents

A "subagent" is:

```text
session(parent_id=<main session>, mode=<worker mode>, background=True)
```

No special subagent code path. No separate memory model. No separate tool protocol.

### TUI Is a Projection

The TUI should not be the runtime. It should render sessions/events/artifacts and send commands. This makes the harness usable from:

- terminal UI
- CLI
- tests/evals
- future API server

For this rewrite, use the Rust/Ratatui workbench that subscribes to the same event projection as the CLI. Do not reintroduce a Python Textual app.

## What a Browser Agent Needs That Coding Agents Usually Do Not

Browser-specific needs:

- active tab/target clarity
- visible screenshot lifecycle
- visual observations as artifacts
- browser profile management
- cookies/storage helpers
- download watcher
- native file picker fallback
- permission prompt handling
- network capture and request replay
- iframe/shadow DOM handling
- coordinate click tooling
- viewport/device emulation
- PDF/document extraction
- table extraction
- OCR fallback
- consent/banner helpers
- anti-bot/captcha/human handoff state
- URL/source provenance
- step counting for tasks that require it

Coding-agent features to de-emphasize:

- git status as a default concern
- review comments
- patch safety language
- repository-wide planning prompts
- complex permission prompts
- codebase indexing

The browser agent still needs coding ability, but as a means to interact with the browser, not as the product identity.

## The Robust/Dynamic Boundary

The cleanest boundary is:

```text
Core runtime: stable, tested, boring
Workspace helpers: flexible, editable, task-specific
```

Core runtime examples:

- auth
- provider stream
- sessions
- event store
- CDP daemon
- tool processes
- artifacts
- compaction

Workspace examples:

- `agent_helpers.py`
- `skills/*.py`
- extraction scripts
- task notes
- generated schemas
- site-specific helpers

The model should rarely need to edit core runtime, but it must always be allowed to.

## Necessary Core vs Promptable

Absolutely necessary in code:

- CDP daemon and target recovery
- persistent Python runtime with raw CDP
- ordered screenshot timelines with model-visible images
- event stream and session store
- event-driven background sessions with cancellation/steering
- simple internal serialization for browser targets, Python kernels, sessions, and file writes
- output spillover and artifacts
- compaction
- Codex auth/refresh
- opencode/Codex-style file tools

Can start as prompting or editable helpers:

- site-specific strategies
- cookie banner handling
- extraction heuristics
- deciding which screenshots to attach
- step-counting conventions
- domain skills
- eval critique style

## Decisions From Current Feedback

1. MVP TUI: Rust/Ratatui, event-projection only. No Python Textual app.
2. Screenshots: attach only when the model asks through `screenshot(..., attach=True)`, but allow multiple ordered images in one tool result.
3. Python runtime: use one persistent interpreter per session. This matches model expectations and supports self-improving helpers.
4. Browser profiles: use a copied harness profile directory per top-level task/run. Do not use the real Chrome folder directly. Shared copied profile is fine.
5. Child sessions/tabs: allow child sessions to control the same tabs by default for freedom. Use text instructions and minimal internal serialization for coordination.
6. File tools: do not expose coding-agent file tools by default. Add them later only if browser tasks prove they need them.
7. Eval: use LLM-based self-eval from the beginning, preferably by spawning normal evaluator sessions over trace bundles.
