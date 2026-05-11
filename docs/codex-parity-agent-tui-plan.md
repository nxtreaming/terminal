# Codex-Parity Agent and TUI Rewrite Plan

Status: implemented locally with live-validation caveats.

## Implementation Status

As of 2026-05-10, the local rewrite implements the plan's core shape:

- SQLite is the durable session/event/artifact store.
- The agent loop routes model calls through a Rust `ToolRegistry`.
- The model-facing tool surface includes Codex-style `exec_command`, `write_stdin`, `apply_patch`, `read_file`, `search_files`, `list_files`, `view_image`, `update_plan`, `spawn_agent`, `send_input`, `wait_agent`, and `close_agent`.
- The Python tool remains the browser island and records browser state, live URLs, artifacts, screenshots/images, and explicit browser identity changes: `browser.connected`, `browser.reconnected`, `browser.disconnected`, and `browser.target_changed`.
- The TUI renders from typed events, uses `pulldown-cmark` for result markdown, has a multiline composer, clamps menu navigation, keeps follow-ups in the current task, and renders setup/actions/history/browser as product screens instead of debug modals.
- Deterministic screen dumps were generated under `/tmp/but-design-loop/rust-tui-final/`.

Latest verification:

```bash
cargo fmt --check
cargo test --workspace
uv run --with pytest python -m pytest -q
```

Known caveats:

- `apply_patch` accepts a raw patch string internally and a `{ "patch": "..." }` object through JSON function-call providers. True provider-level freeform patches still depend on provider/tool-call support.
- Browser Use cloud mode and full live-provider dataset regression require live credentials and were not re-run in this final local pass.
- The hidden developer trace still exists for diagnostics, but it is removed from the default actions/help product surface.

This plan merges two goals:

1. Give the browser agent the same raw abilities and workflow feel as Codex.
2. Rebuild the terminal UI around the product UX in `docs/terminal-ui-product-ux.md`.

The important constraint is that we should copy Codex at the agent interface layer, not copy Codex internals wholesale.

We want the model to experience nearly the same tool surface, tool behavior, editing flow, command flow, and subagent affordances as Codex. We do not want Codex's full sandboxing, permission policy engine, app-server protocol, enterprise hooks, or internal complexity.

## Goal Statement

Build a browser agent that feels like Codex plus a reliable browser harness:

```text
+---------------------+       +--------------------+       +---------------------+
| Model               | ----> | Rust tool registry | ----> | Typed event store   |
| Codex-like tools    |       | Simple internals   |       | SQLite transcript   |
+---------------------+       +--------------------+       +---------------------+
                                      |
                                      v
                         +-------------------------+
                         | Tool backends           |
                         |                         |
                         | coding tools in Rust    |
                         | browser tool in Python  |
                         | subagents in Rust       |
                         +-------------------------+
                                      |
                                      v
                         +-------------------------+
                         | TUI transcript renderer |
                         | Product-first workbench |
                         +-------------------------+
```

Done means:

- the model can inspect, edit, run, and verify code like Codex
- the model can control the browser through the Python browser harness
- the user sees a simple browser-agent UI, not an internal dashboard
- every important action is stored as a typed event and can be rendered, resumed, or debugged

## Non-Goals

Do not implement these in the first rewrite:

- Codex sandboxing
- permission prompts
- prefix policy engine
- guardian process
- Codex app-server protocol
- enterprise hooks
- complex approval modes
- browser lifecycle ownership in Rust
- a generic plugin system
- a debug dashboard as the main product
- separate file-based session state

These can be added later if they become necessary.

## Core Product Shape

The app should be one workbench with overlays:

```text
FIRST RUN -> SETUP -> READY

READY -> RUNNING -> RESULT
          |          |
          |          v
          |       FOLLOW-UP
          |
          v
       STEER / STOP

BROKEN SETUP -> SETUP REPAIR -> READY
FAILED TASK  -> RETRY / FIX   -> READY
```

The main user vocabulary is:

```text
task
browser
account
model
result
history
setup
```

Avoid exposing these as first-class product concepts in the default UI:

```text
session
artifact
trace
provider
config
compact
event
tool output
```

Those can exist internally.

## Target Agent Tool Surface

The model-facing tool interface should be as close to Codex as practical.

### 1. `exec_command`

Purpose: run a shell command in a PTY or pipe mode.

Arguments should closely match Codex:

```text
cmd
workdir
shell
tty
login
yield_time_ms
max_output_tokens
```

Behavior:

- run commands from the current workspace
- stream output as events
- return final output if the process exits quickly
- return `session_id` if the process is still running
- support long-running processes like dev servers, tests, REPLs, and watchers
- make output truncation explicit
- emit a useful error when the command cannot start

Acceptance:

- the agent can run `cargo test`, `pytest`, `rg`, `npm run dev`, and long-running server commands
- long-running commands do not block the whole agent forever
- the TUI shows command started, output, still running, finished, or failed

### 2. `write_stdin`

Purpose: interact with a still-running command session.

Arguments:

```text
session_id
chars
yield_time_ms
max_output_tokens
```

Behavior:

- send input to a persistent command session
- poll output with empty input
- support cancellation/termination later
- preserve PTY behavior where required

Acceptance:

- the agent can answer prompts in a CLI
- the agent can interact with REPLs
- the agent can poll a dev server or long-running test job

### 3. `apply_patch`

Purpose: make file edits through a Codex-style freeform patch tool.

Behavior:

- freeform tool, not JSON wrapped
- support add file
- support delete file
- support update file
- support move file
- produce clean parse and application errors
- emit typed file-change events while applying
- preserve this as the primary edit path

Acceptance:

- the agent can edit multiple files in one patch
- failed patches are understandable and recoverable
- the TUI can render changed files and patch result

### 4. `read_file`

Purpose: read local files without shell hacks.

Arguments:

```text
path
start_line
end_line
max_bytes or max_lines
```

Behavior:

- read text files with line numbers when useful
- tolerate invalid UTF-8 with replacement
- detect binary files
- truncate clearly
- support focused ranges

Acceptance:

- the agent can inspect files efficiently
- the agent does not need `sed` or `cat` for every file read
- large files do not flood model context

### 5. `search_files`

Purpose: fast repo search.

Implementation:

- use `rg` first
- fall back only if `rg` is unavailable

Arguments:

```text
query
path
glob
context_lines
max_results
```

Behavior:

- respect ignored files by default
- return compact file/line matches
- cap output
- make truncation obvious

Acceptance:

- the agent can find symbols, tests, docs, and references quickly
- the tool output is compact enough for repeated use

### 6. `list_files` / fuzzy file search

Purpose: discover files by path/name.

Implementation options:

- Rust `ignore` crate for walking
- Rust fuzzy matcher like `nucleo` if we want Codex-like file search

Behavior:

- list files under a root
- optionally fuzzy match a pattern
- ignore build outputs and hidden dependency trees by default

Acceptance:

- the agent can discover repo structure without dumping enormous trees

### 7. `view_image`

Purpose: let the model inspect local screenshots and image artifacts.

Behavior:

- accept local image path
- attach image to the next model turn if the provider supports it
- otherwise use the existing image-input fallback
- emit artifact event for the TUI

Acceptance:

- browser screenshots and local screenshots are inspectable by the model
- the TUI can show image artifacts or links to them

### 8. Python browser tool

Purpose: own browser connection, lifecycle awareness, and browser task execution.

Ownership:

- Python owns or is deeply aware of the browser connection
- Rust does not duplicate CDP target/session/object state
- Rust sees the browser as a tool backend with typed events

Behavior:

- persistent Python runtime
- raw CDP access
- browser helper functions
- screenshot capture
- artifact upload/download
- reconnect detection
- tab/target changes surfaced explicitly
- object ID and target ID invalidation handled in the Python/browser layer

Important events:

```text
browser.connected
browser.reconnected
browser.disconnected
browser.target_changed
browser.live_url
browser.screenshot
browser.artifact
browser.action
browser.error
```

Acceptance:

- reconnects do not silently corrupt browser context
- the model is told when target IDs/object IDs may be stale
- live browser opens from the TUI
- browser tasks can return screenshots in the model-visible continuation

### 9. `update_plan`

Purpose: let the model externalize progress.

Behavior:

- simple step/status list
- no complex goal machinery needed initially
- stored as events
- rendered in the running transcript if present

Acceptance:

- long tasks show understandable progress
- plan updates do not become the whole UI

### 10. Subagent tools

Start with the simple Codex-style v1 surface:

```text
spawn_agent
send_input
wait_agent
close_agent
```

Behavior:

- child agent is a child thread/session
- child has its own event log
- parent gets a compact final result
- child can use the same coding/browser tools if allowed
- avoid copying Codex's full v2 mailbox system initially

Acceptance:

- the model can delegate repo exploration
- parent context does not get flooded by all child exploration
- the TUI can show that a helper agent ran and summarize the result

## Internal Architecture

The internals should stay small:

```text
ModelProvider
  streams model items and tool calls

AgentLoop
  owns turn execution
  passes tool calls to ToolRegistry
  passes tool outputs back to model

ToolRegistry
  validates args
  dispatches to tool backend
  emits typed events
  returns model-visible output

ToolBackends
  exec/write_stdin
  apply_patch
  files/search
  Python browser
  subagents
  plan

EventStore
  SQLite append-only event log
  query by task/thread
  replay for TUI

TUI
  renders events
  sends user input/control events
```

The TUI should not own agent logic.

The browser layer should not be split between Rust and Python in a way where both think they own CDP state.

## Event Model

Use typed events as the only durable source of truth.

Minimum event groups:

```text
task.created
task.input
task.followup
task.started
task.finished
task.failed
task.cancelled

model.started
model.delta
model.tool_call
model.finished

tool.started
tool.output_delta
tool.finished
tool.failed

command.started
command.output
command.waiting
command.finished
command.failed

patch.started
patch.file_changed
patch.finished
patch.failed

file.read
file.search
file.list

browser.connected
browser.reconnected
browser.disconnected
browser.live_url
browser.action
browser.screenshot
browser.artifact
browser.error

agent.spawned
agent.message
agent.finished
agent.failed

plan.updated
```

Rules:

- events are append-only
- UI state is derived from events
- task status is derived or stored as a compact index, not hand-maintained in multiple places
- final result is an event
- command output can be chunked
- artifacts are referenced by path/ID, not stored inline in every event

## TUI Product Requirements

The TUI should match `docs/terminal-ui-product-ux.md`.

### First-run setup

First launch should show setup, not a useless empty app.

```text
+--------------------------------------------------------------------------------+
| browser-use                                                                    |
|--------------------------------------------------------------------------------|
| Set up the browser agent                                                       |
|                                                                                |
| [1] Sign in                                                                    |
|     Not connected                                                              |
|                                                                                |
| [2] Choose model                                                               |
|     No model selected                                                          |
|                                                                                |
| [3] Choose browser                                                             |
|     Local Chrome available                                                     |
|                                                                                |
| > Start setup                                                                  |
|                                                                                |
| enter continue                                                                 |
+--------------------------------------------------------------------------------+
```

Acceptance:

- setup blocks running a task if model/auth/browser are unusable
- pressing enter does not accidentally start a blank task
- each setup item leads to a real flow

### Sign in

```text
+--------------------------------------------------------------------------------+
| Sign in                                                                        |
|--------------------------------------------------------------------------------|
| Choose how the agent should connect to a model.                                |
|                                                                                |
| > Codex login                                                                  |
|   Claude Code login                                                            |
|   OpenAI API key                                                               |
|   Anthropic API key                                                            |
|   OpenRouter API key                                                           |
|                                                                                |
| Already connected                                                              |
|   Browser Use cloud key                                                        |
|                                                                                |
| enter select     esc back                                                      |
+--------------------------------------------------------------------------------+
```

Acceptance:

- the user chooses the account path they have
- failed login states offer a clear next action
- provider internals are hidden from the main product language

### Model selection

Model and account path are selected together.

```text
+--------------------------------------------------------------------------------+
| Choose model                                                                   |
|--------------------------------------------------------------------------------|
| Recommended                                                                    |
|                                                                                |
| > GPT-5.5                         Codex login             best default         |
|   GPT-5.5                         OpenAI API key          sign in required     |
|   Claude Opus 4.7                 Claude Code login       sign in required     |
|   Claude Sonnet 4.6               Claude Code login       sign in required     |
|   Qwen3.6 Plus                    OpenRouter API key      sign in required     |
|   GLM-5.1                         OpenRouter API key      sign in required     |
|   DeepSeek V4 Pro                 OpenRouter API key      sign in required     |
|                                                                                |
| Current                                                                        |
|   none                                                                         |
|                                                                                |
| enter select     a sign in     esc back                                        |
+--------------------------------------------------------------------------------+
```

Acceptance:

- do not make users pick provider first
- do not show a model that cannot run without explaining what account it needs
- failed random OpenRouter model setup has a useful recovery path
- default model list is curated, not a model zoo

### Browser selection

```text
+--------------------------------------------------------------------------------+
| Choose browser                                                                 |
|--------------------------------------------------------------------------------|
| > Local Chrome                 visible browser on this machine                 |
|   Browser Use cloud            remote browser with live view                   |
|   Headless Chromium            background browser                              |
|                                                                                |
| Current                                                                        |
|   Local Chrome available                                                       |
|                                                                                |
| enter select     esc back                                                      |
+--------------------------------------------------------------------------------+
```

Acceptance:

- local Chrome, remote Browser Use cloud, and headless Chromium are understandable choices
- live browser can actually be opened
- browser reconnect can actually be triggered

### Ready workbench

```text
+--------------------------------------------------------------------------------+
| browser-use                                               local chrome  gpt-5.5 |
|                                                                                |
| What should the browser do?                                                     |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Find the top 5 Hacker News posts                                          | |
| +----------------------------------------------------------------------------+ |
|                                                                                |
| Recent                                                                         |
|   > found Hacker News top posts                                  12m ago       |
|   > compared 4 pricing pages                                    yesterday      |
|                                                                                |
| Ready                                                                          |
|   signed in      browser connected                                             |
|                                                                                |
| enter run     tab history     / actions     f1 keys                            |
+--------------------------------------------------------------------------------+
```

Acceptance:

- clear task entry
- recent work is visible
- readiness is visible
- no giant logo
- no internal debug dashboard

### Running task

```text
+--------------------------------------------------------------------------------+
| find the top 5 Hacker News posts                            running  $0.03  48s |
|--------------------------------------------------------------------------------|
|                                                                                |
|   * browsing   news.ycombinator.com                                            |
|   * reading    front page                                                      |
|   * found      5 posts                                                         |
|   * checking   scores and comments                                             |
|                                                                                |
| Browser                                                                        |
|   page       https://news.ycombinator.com/                                     |
|   open       live browser                                                      |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Type to steer the agent...                                                | |
| +----------------------------------------------------------------------------+ |
|                                                                                |
| enter steer     ctrl+c stop     f2 browser     / actions                       |
+--------------------------------------------------------------------------------+
```

Acceptance:

- running state answers what it is doing
- browser state is visible
- user can steer
- user can stop
- raw tool noise is summarized unless expanded

### Result view

The result view must show the whole task story:

```text
+--------------------------------------------------------------------------------+
| find the top 5 Hacker News posts                               done  $0.06  2m |
|--------------------------------------------------------------------------------|
| Task                                                                           |
|   find the top 5 Hacker News posts                                             |
|                                                                                |
| Steps                                                                          |
|   * browsed news.ycombinator.com                                               |
|   * read front page                                                            |
|   * extracted 5 posts                                                          |
|   * saved hackernews_top5.json                                                 |
|                                                                                |
| Result                                                                         |
|                                                                                |
| Top 5 Hacker News posts                                                        |
|                                                                                |
| 1. Example title                                                               |
|    299 points, 300 comments                                                    |
|    https://example.com                                                         |
|                                                                                |
| Source                                                                         |
|   https://news.ycombinator.com/news                                            |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Ask a follow-up...                                                        | |
| +----------------------------------------------------------------------------+ |
|                                                                                |
| enter follow-up     f2 browser     tab history     / actions                   |
+--------------------------------------------------------------------------------+
```

Acceptance:

- result markdown renders correctly
- links are clickable
- task + steps + final result are all visible
- source links are visible
- telemetry can be shown compactly
- follow-up continues the current thread

### Failure view

```text
+--------------------------------------------------------------------------------+
| analyse the current repository                                   failed  $0.02 |
|--------------------------------------------------------------------------------|
| The agent could not reach the model.                                           |
|                                                                                |
| Read timed out while connecting to chatgpt.com.                                |
|                                                                                |
| > Retry                                                                        |
|   Sign in                                                                      |
|   Choose model                                                                 |
|   Change browser                                                               |
|   Stop                                                                         |
|                                                                                |
| Work preserved in history.                                                     |
|                                                                                |
| enter select     esc back                                                      |
+--------------------------------------------------------------------------------+
```

Acceptance:

- every failure offers a useful next action
- failed auth/model states never dump the user into a dead end

### Browser overlay

```text
+--------------------------------------------------------------------------------+
| Browser                                                                        |
|--------------------------------------------------------------------------------|
| Current                                                                        |
|   backend      local chrome                                                    |
|   title        Hacker News                                                     |
|   page         https://news.ycombinator.com/                                   |
|   status       connected                                                       |
|   tabs         1 open                                                          |
|   viewport     1440 x 900                                                      |
|                                                                                |
| > Open browser                                                                 |
|   Reconnect                                                                    |
|   Change browser                                                               |
|                                                                                |
| enter select     esc close                                                     |
+--------------------------------------------------------------------------------+
```

Acceptance:

- F2 opens this overlay
- open browser works
- reconnect works
- change browser works
- raw CDP details stay hidden by default

### History overlay

```text
+--------------------------------------------------------------------------------+
| Previous work                                                                  |
|--------------------------------------------------------------------------------|
| > find the top 5 Hacker News posts                          done      12m ago  |
|   compare browser automation tools                          done      1h ago   |
|   analyse repository structure                              failed    2h ago   |
|                                                                                |
| enter open     r resume     esc close                                          |
+--------------------------------------------------------------------------------+
```

Acceptance:

- Tab opens history
- selection clamps at top and bottom
- opening previous work loads the selected task
- resume/follow-up semantics are clear

### Actions menu

```text
+--------------------------------------------------------------------------------+
| Actions                                                                        |
|--------------------------------------------------------------------------------|
| > New task                                                                     |
|   Open browser                                                                 |
|   Previous work                                                                |
|   Setup                                                                        |
|   Choose model                                                                 |
|                                                                                |
| type to search     enter select     esc close                                  |
+--------------------------------------------------------------------------------+
```

Acceptance:

- slash opens actions
- menu selection clamps at top/bottom
- actions actually perform the selected behavior
- no generic debug menu in the default product

## Composer Requirements

The composer should feel native and Codex-like.

Behavior:

- multiline input
- grows up to about 10 lines
- `enter` submits
- `shift+enter` inserts newline
- cursor is visible and accurate
- mouse wheel/scroll never moves the composer cursor
- scroll only scrolls transcript/history
- `cmd+delete` clears the current line
- repeated `cmd+delete` clears previous lines one by one
- option-delete deletes previous word
- ctrl-a / ctrl-e or home/end work if supported by the terminal framework
- left/right/up/down work naturally inside multiline input
- empty composer arrow keys can navigate transcript/history only if that does not break text editing
- follow-up composer submits into the current task thread

Acceptance:

- composer behavior matches normal terminal text editing expectations
- no cursor spacing bugs after up/down movement
- no scroll-to-cursor coupling
- no accidental new task when submitting a follow-up

## Markdown Rendering Requirements

Use a real Markdown parser.

Preferred implementation:

- `pulldown-cmark` for parsing
- a small terminal renderer that maps markdown nodes to Ratatui/Textual spans

Required support:

- paragraphs
- headings
- bold
- italic if cheap
- inline code
- fenced code blocks
- ordered lists
- unordered lists
- links
- autolinks
- hard/soft line breaks

Clickable behavior:

- URLs should be selectable/clickable when the terminal supports it
- file paths should be rendered clearly
- source links should not be hidden inside raw markdown syntax

Acceptance:

- final output no longer shows raw `**bold**` for common markdown
- bullet wrapping is correct
- long links wrap without breaking the layout

## Implementation Sequence

### Phase 0: Audit and lock the copied surface

Purpose: decide exactly what we copy from Codex and what we preserve from the current Python tools.

Status: completed in `docs/tool-compatibility-audit.md`.

Steps:

1. Audit Codex tool contracts:
   - `exec_command`
   - `write_stdin`
   - `apply_patch`
   - `view_image`
   - subagent tools
   - planning tool
2. Audit current main-branch Python coding tools:
   - shell
   - file read/edit
   - grep/glob
   - patch/write
   - session/subagent tools
3. Produce a tool compatibility table:
   - tool name
   - Codex schema
   - current schema
   - target schema
   - implementation backend
4. Decide exact naming:
   - prefer Codex names where possible
   - keep browser-specific names only where Codex has no equivalent

Exit criteria:

- one checked-in tool surface table
- no implementation begins until the target tool list is explicit

### Phase 1: SQLite event store

Purpose: replace fragile file session state with an event store suitable for replay and UI rendering.

Steps:

1. Define SQLite schema:
   - tasks
   - events
   - artifacts
   - command_sessions
   - optional indexes for status/recent history
2. Implement append-only event writes.
3. Implement event replay by task.
4. Implement recent task query.
5. Keep migration path from old JSONL/file sessions if needed.

Exit criteria:

- creating a task writes events
- reloading a task reconstructs the transcript
- history view can query recent tasks

### Phase 2: Tool registry substrate

Purpose: one central place for tools.

Steps:

1. Define `ToolSpec`.
2. Define `ToolCall`.
3. Define `ToolOutput`.
4. Define `ToolInvocation`.
5. Define `ToolRegistry`.
6. Add validation and typed error responses.
7. Emit `tool.started`, `tool.finished`, and `tool.failed` around every tool.
8. Make the model loop route all tool calls through this registry.

Exit criteria:

- fake provider can call fake tools through the registry
- real provider can see registered tool schemas
- every call produces events

### Phase 3: Codex-like command runtime

Purpose: give the agent real terminal ability.

Steps:

1. Implement `exec_command`.
2. Implement persistent process/session table.
3. Implement output buffering and truncation.
4. Implement PTY mode.
5. Implement `write_stdin`.
6. Add cancellation/termination path.
7. Add event rendering for command begin/output/end.

Exit criteria:

- model can run tests
- model can run long-lived commands
- model can poll or send stdin
- TUI shows command progress without corrupting the composer

### Phase 4: Codex-like file and edit tools

Purpose: restore strong coding ability.

Steps:

1. Implement or vendor Codex-compatible `apply_patch`.
2. Implement `read_file`.
3. Implement `search_files`.
4. Implement `list_files` or fuzzy file search.
5. Implement `view_image`.
6. Add file-change event rendering.
7. Add focused tests for:
   - add/update/delete/move patch
   - patch failure
   - range reads
   - binary read rejection
   - grep truncation

Exit criteria:

- model can inspect and edit repo files without shell hacks
- patches are visible in the TUI
- tests cover common edit failures

### Phase 5: Python browser backend

Purpose: preserve the browser harness strength.

Steps:

1. Define Rust-to-Python browser protocol.
2. Keep Python as owner of browser connection awareness.
3. Surface reconnects as explicit events.
4. Surface target/tab changes as explicit events.
5. Return screenshots as model-visible images when possible.
6. Store screenshots as artifacts.
7. Implement `open live browser` action.
8. Implement browser overlay data source.

Exit criteria:

- browser tasks run through the new registry
- reconnects are not silent
- live browser opens
- browser overlay reflects real state

### Phase 6: Model loop integration

Purpose: make Codex-like tools the normal agent path.

Steps:

1. Generate model tool specs from the registry.
2. Feed tool outputs back into the model.
3. Preserve ordered image outputs from browser tools.
4. Add compaction/truncation only where necessary.
5. Make follow-ups continue the same task thread.
6. Make new task explicitly create a new task.

Exit criteria:

- a real model can run a browser task
- a real model can run a coding task
- follow-up behavior is correct
- no special browser-only model path exists

### Phase 7: Subagents

Purpose: give Codex-like delegation without flooding parent context.

Steps:

1. Implement `spawn_agent`.
2. Implement `send_input`.
3. Implement `wait_agent`.
4. Implement `close_agent`.
5. Store child tasks as child task records.
6. Return compact child summaries to parent.
7. Render child-agent activity as compact transcript blocks.

Exit criteria:

- model can delegate repository exploration
- parent context receives summary, not full child logs
- child work is inspectable from history/debug path

### Phase 8: TUI rewrite on top of events

Purpose: make the UI product-first and stable.

Steps:

1. Implement app state as a projection of events.
2. Implement first-run setup.
3. Implement setup repair overlay.
4. Implement ready workbench.
5. Implement running task view.
6. Implement result view.
7. Implement failure view.
8. Implement browser overlay.
9. Implement history overlay.
10. Implement actions menu.
11. Implement Codex-like multiline composer.
12. Implement markdown rendering with `pulldown-cmark`.
13. Implement clickable links where the terminal framework supports it.
14. Ensure scroll only affects transcript/history.

Exit criteria:

- all screens in this doc can be reached
- all menus clamp correctly
- composer passes keyboard behavior tests
- result markdown renders properly
- no product-critical action is a dead button

### Phase 9: Verification loop

Purpose: prevent subjective UI regressions.

Steps:

1. Add deterministic TUI tests for:
   - setup
   - ready workbench
   - running task
   - result view
   - failure view
   - browser overlay
   - history overlay
   - actions menu
   - multiline composer
2. Save screenshots under `/tmp/but-design-loop/`.
3. Verify key input behavior:
   - enter
   - shift+enter
   - cmd+delete
   - option+delete
   - tab
   - slash
   - f2
   - esc
   - ctrl+c
   - mouse wheel
4. Run browser-tool tests when browser events or live preview change.
5. Run workspace tests before commits.

Exit criteria:

- screenshots match intended layouts
- keyboard behavior is verified
- browser live preview is verified
- no known broken core screen remains

## Suggested Milestones

### Milestone A: Agent can code like Codex

Includes:

- tool registry
- `exec_command`
- `write_stdin`
- `apply_patch`
- `read_file`
- `search_files`
- `list_files`
- basic event rendering

This should happen before deep TUI polish.

### Milestone B: Browser backend is cleanly integrated

Includes:

- Python browser bridge
- reconnect events
- screenshots as model-visible outputs
- live browser action
- browser overlay data

### Milestone C: TUI product shell works

Includes:

- setup
- ready workbench
- running
- result
- failure
- history
- actions
- browser overlay
- composer behavior
- markdown rendering

### Milestone D: Subagents

Includes:

- spawn
- send input
- wait
- close
- compact result handoff
- child task visibility

### Milestone E: Hardening

Includes:

- deterministic screenshots
- keyboard tests
- browser reconnect tests
- long-running command tests
- markdown wrapping tests
- result/follow-up tests

## Recommended Goal Wording

Use this as the tracking goal:

```text
Create a Codex-parity browser agent rewrite:

1. audit Codex and existing Python coding tools
2. define the final Codex-like tool surface
3. implement a simple Rust tool registry and SQLite event store
4. implement Codex-like coding tools without sandboxing or permissions
5. integrate the Python-owned browser backend as a tool backend
6. route the model loop through the registry
7. add simple Codex-style subagents
8. rebuild the TUI around the product UX and typed event transcript
9. verify core screens and keyboard behavior with deterministic TUI tests

Do not copy Codex internals that are only needed for sandboxing, permissions, app-server protocol, or enterprise policy.
```

## First Implementation Task

Do this first:

```text
Audit Codex and current Python tools, then write the target tool compatibility table.
```

Output should be a small checked-in document or section that answers:

- what tools the model gets
- exact tool names
- exact arguments
- which implementation backend owns each tool
- which tools are copied from Codex behavior
- which tools are preserved from current Python behavior
- which tools are deliberately not included

Only after that should implementation start.
