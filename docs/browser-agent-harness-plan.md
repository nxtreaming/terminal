# Browser Agent Harness Implementation Plan

Status: design draft for iteration. The implementation split is tracked in `docs/implementation-roadmap.md`.

This plan is for a new browser-specific LLM harness built from scratch in this repo. The harness should preserve the useful substrate ideas from the existing browser harness, BU agent package, opencode, pi-mono, Codex, Hermes, and OpenClaw, while removing the coding-agent baggage that slows down browser work.

The core decision: build opencode plus the browser harness as one product. Keep opencode's durable session/event/subagent shape, but replace the coding-agent-centered tool surface with the harnesless browser idea: one reliable CDP substrate, raw browser freedom, and an agent-editable helper layer.

The model should get a small set of powerful primitives, especially a persistent Python browser runtime with raw CDP access, a shell escape hatch, file editing, session control, and normal background sessions. Everything above that should be agent-editable and replaceable.

The screenshot-read problem is a first-class requirement. The browser/Python tool must be able to return screenshots as model-visible image outputs in the same tool result that performed the browser actions, so the next model continuation can see the page without a separate "take screenshot, then view image" tool round trip.

## Sources Reviewed

Local spec and datasets:

- `spec.md`
- `datasets/real_v8.json`
- `datasets/real_v14_short.json`

Browser harness:

- `/Users/greg/Documents/browser-use/hackathons/harnesless/README.md`
- `/Users/greg/Documents/browser-use/hackathons/harnesless/SKILL.md`
- `/Users/greg/Documents/browser-use/hackathons/harnesless/src/browser_harness/run.py`
- `/Users/greg/Documents/browser-use/hackathons/harnesless/src/browser_harness/helpers.py`
- `/Users/greg/Documents/browser-use/hackathons/harnesless/src/browser_harness/daemon.py`
- `/Users/greg/Documents/browser-use/hackathons/harnesless/src/browser_harness/_ipc.py`
- `/Users/greg/Documents/browser-use/hackathons/harnesless/src/browser_harness/admin.py`
- `/Users/greg/Documents/browser-use/hackathons/harnesless/agent-workspace/agent_helpers.py`

BU package:

- `/Users/greg/Documents/browser-use/core/cloud/packages/bu/bu_use/agent/service.py`
- `/Users/greg/Documents/browser-use/core/cloud/packages/bu/bu_use/agent/events.py`
- `/Users/greg/Documents/browser-use/core/cloud/packages/bu/bu_use/agent/output_truncation.py`
- `/Users/greg/Documents/browser-use/core/cloud/packages/bu/bu_use/agent/compaction/*`
- `/Users/greg/Documents/browser-use/core/cloud/packages/bu/bu_use/tools/decorator.py`
- `/Users/greg/Documents/browser-use/core/cloud/packages/bu/bu_use/tools/depends.py`
- `/Users/greg/Documents/browser-use/core/cloud/packages/bu/bu_use/bu/service.py`
- `/Users/greg/Documents/browser-use/core/cloud/packages/bu/bu_use/bu/tools/subagent*.py`

opencode:

- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/tool/task.ts`
- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/tool/task.txt`
- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/agent/agent.ts`
- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/v2/session.ts`
- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/session/compaction.ts`
- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/tool/truncate.ts`
- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/plugin/codex.ts`
- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/session/llm.ts`
- `/Users/greg/Downloads/tmp/opencode/packages/opencode/src/session/processor.ts`

pi-mono:

- `/Users/greg/Downloads/tmp/pi-mono/packages/agent/src/agent-loop.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/agent/src/agent.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/agent/src/types.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/coding-agent/src/core/agent-session.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/coding-agent/src/core/agent-session-runtime.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/coding-agent/src/core/session-manager.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/ai/src/providers/openai-codex-responses.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/ai/src/utils/oauth/openai-codex.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/coding-agent/src/core/auth-storage.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/coding-agent/src/core/tools/output-accumulator.ts`
- `/Users/greg/Downloads/tmp/pi-mono/packages/coding-agent/src/core/tools/truncate.ts`

Codex:

- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/session/session.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/session/turn.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/tools/orchestrator.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/tools/router.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/tools/handlers/unified_exec.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/unified_exec/process_manager.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/compact.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/login/src/auth/manager.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/login/src/auth/storage.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/login/src/server.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/login/src/pkce.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/chatgpt/src/chatgpt_client.rs`

Hermes/OpenClaw auth references:

- `/Users/greg/Downloads/tmp/hermes-agent/hermes_cli/auth.py`
- `/Users/greg/Downloads/tmp/hermes-agent/agent/transports/codex.py`
- `/Users/greg/Downloads/tmp/hermes-agent/agent/codex_responses_adapter.py`
- `/Users/greg/Downloads/tmp/hermes-agent/agent/credential_pool.py`
- `/Users/greg/Downloads/tmp/openclaw/src/agents/cli-credentials.ts`
- `/Users/greg/Downloads/tmp/openclaw/src/agents/model-auth.ts`

External reading:

- Browser Use article: `https://browser-use.com/posts/bitter-lesson-frameworks` (the user-provided URL appears to point at the same idea with a different slug).

## Product Target

The end product is a terminal app for controlling Chrome through an LLM:

- It starts or attaches to Chrome through CDP.
- It shows active browser/session state in a terminal UI.
- It can create, resume, switch, and stop sessions.
- Each session is a normal agent session, including "subagents"; there is no separate special subagent abstraction.
- The LLM can run persistent Python snippets with CDP helpers preloaded.
- The LLM can run shell commands, edit files, inspect artifacts, and write helper code.
- The harness stores screenshots, optional narrow page extracts, downloads, network traces, tool output, and session history as durable artifacts.
- The TUI is mostly a viewer/controller over an event stream. It should not own core agent logic.

The initial benchmark target is the provided task datasets:

- `real_v8.json`: easier, broad browser tasks.
- `real_v14_short.json`: harder tasks, more dynamic pages, longer extraction, screenshots, documents, pagination, logins/consent/UI blockers.

## First Principles

### 1. A browser is not a DOM API

A browser task can require:

- page DOM inspection
- screenshots and visual reasoning
- real input events
- iframes
- shadow DOM
- cross-origin frames
- downloads
- native file pickers
- permission prompts
- HTTP requests
- local files
- cookies/local storage/session storage
- PDFs
- popups and tabs
- Chrome UI state
- OS-level automation when CDP gets stuck

Any harness that says "click this selector" as the central primitive will fail on part of that space. The correct base primitive is a live Chrome process with raw CDP access, plus the ability to write code around CDP while the task is running.

### 2. Complete freedom needs a reliable substrate

"Let the model do anything" does not mean the harness should be sloppy. It means:

- substrate-level things must be hard to break
- the model-visible conceptual surface must stay small
- high-level helpers must be editable and replaceable
- the model must always have escape hatches

The robust parts are connection management, process/session lifecycle, auth, event storage, compaction, output truncation, artifact handling, cancellation, and streaming. The dynamic parts are browser interaction recipes, extraction helpers, site-specific hacks, and task-level code.

### 3. Latency is mostly round trips

The current coding-harness style is slow for browser work because a task often becomes:

1. tool call to click
2. model call
3. tool call for screenshot
4. model call
5. tool call for DOM
6. model call

The new harness should make one tool call capable of doing an entire mini-plan:

- navigate
- wait
- inspect DOM
- inspect network
- click or type
- capture screenshot
- parse/extract
- save artifacts
- return compact state

The model can write and run a Python snippet to execute that mini-plan in one go. Screenshots should be available as tool-result attachments/artifacts and in the TUI without forcing an extra LLM turn just to see them.

### 4. Screenshots must be data returned from tools, not separate work

The problem to eliminate is not the standard model continuation after a tool call. Tool calling always needs the model to continue after the application executes the tool. The problem is the extra browser-tool or image-viewer tool call just to make a screenshot visible to the model.

The target flow is:

```text
model calls python tool:
  step 1: navigate
  step 1: capture screenshot
  step 2: click/type/scroll
  step 2: capture screenshot
  step 3: wait/extract
  step 3: capture screenshot
  return text + ordered image timeline + artifact ids

harness sends function_call_output back to model:
  output: [
    {"type": "input_text", "text": "Step 1: opened page ...\nStep 2: clicked checkout ..."},
    {"type": "input_image", "image_url": "data:image/png;base64,...", "detail": "auto"},
    {"type": "input_image", "image_url": "data:image/png;base64,...", "detail": "auto"},
    {"type": "input_image", "image_url": "data:image/png;base64,...", "detail": "auto"}
  ]

model continues:
  it can inspect the temporal sequence immediately
```

The public OpenAI Responses API documents function tool outputs as text, image, or file content outputs, including `input_image` entries. See `https://platform.openai.com/docs/api-reference/responses`. The Codex subscription backend should be smoke-tested because it is a ChatGPT backend endpoint, not the ordinary public endpoint; if it rejects image content in function outputs, the fallback is to insert a synthetic user/input item with the screenshot immediately after the tool output inside the same agent loop. Either way, the model must not need a separate screenshot-reading tool call.

The Python browser tool should support:

```python
screenshot("after_click", attach=True)
emit_image(path_or_bytes, detail="auto")
screenshot("after_login_click", attach=True)
```

`attach=True` means "include this image in the next model continuation," not merely "save it to disk." The image is still saved as an artifact for replay and TUI display.

The temporal component matters. The return format should preserve order, labels, timestamps, URL/title, viewport, and artifact path for every attached image. A long browser tool call should be able to emit:

```text
1. before_click.png
2. after_click.png
3. after_network_idle.png
4. final_state.png
```

The model should see all selected frames, not only the last screenshot. The TUI should receive these frames as streaming tool events while the tool is still running, so the human can verify progress and cancel if needed. The LLM sees the ordered image timeline on the next continuation.

Do not attach whole DOM trees by default. DOM is useful only when the model explicitly asks for a narrow extraction, such as selected text, a small accessibility slice, a list of links, form fields, or a custom JS result. The browser harness works well because it avoids pretending a page is just a DOM tree; keep that property.

### 5. The model knows code better than custom browser wrappers

Modern models can write CDP calls, JavaScript snippets, curl requests, AppleScript, and Python helper functions. The harness should bias toward:

- raw CDP exposed as `cdp(method, params)`
- compact helpers for common actions
- editable helper files
- editable tool files and reloadable tool registry
- a persistent Python runtime
- low ceremony around trying alternate approaches

It should not bias toward a large fixed catalog of semantic browser tools.

The first examples shown to the model should be raw CDP examples, not only helper examples:

```python
cdp("Page.navigate", {"url": "https://example.com"})
cdp("Runtime.evaluate", {
    "expression": "document.body.innerText",
    "returnByValue": True,
})
cdp("Input.dispatchMouseEvent", {
    "type": "mousePressed",
    "x": 320,
    "y": 220,
    "button": "left",
    "clickCount": 1,
})
```

Helpers like `new_tab()`, `js()`, and `click_at()` are starter code. The model must be able to open their source, edit them, reload them, or ignore them and call pure CDP directly.

### 6. Tool concurrency should stay simple

Do not build a complicated scheduling language for MVP. The important part is much simpler:

- Run independent read-only tools in parallel.
- Serialize tools that mutate the same obvious state.
- Keep this mostly invisible to the model.

Internal serialization is enough:

- one persistent Python/browser action at a time per session
- one active CDP mutation at a time per target/profile
- one file write/edit/apply_patch at a time per file
- session mutations serialized per session

No model-facing `barrier` primitive should exist in v1 unless real traces prove it is needed. If the model needs strict ordering, it can put ordered browser actions inside one Python call, which is exactly the browser-harness style.

### 7. Session is the primitive

A session is the unit of:

- conversation history
- browser/profile binding
- artifact storage
- event stream
- model/provider settings
- current tool runtimes
- compaction state
- child/parent relationship

Subagents are just sessions with `parent_id`, a different instruction/mode, and possibly a background run. This is the opencode lesson. It avoids a second parallel framework for subagents.

## Architecture Summary

The system should have four layers:

```text
Terminal UI
  subscribes to event stream, displays sessions/browser/tools/artifacts

Agent Runtime
  sessions, agent loop, tools, compaction, model provider, artifacts

Browser Runtime
  Chrome process/profile, CDP daemon, target routing, screenshots, downloads

Agent Workspace
  editable helpers, domain skills, task scratch files, generated extraction code
```

The agent runtime should not depend on the TUI. The browser runtime should not depend on the model provider. The workspace should be reloadable without restarting the harness.

## Proposed Package Structure

Use the BU package shape as inspiration, but remove the BU layer and expose only the agent/browser harness.

```text
src/
  agent/
    __init__.py
    cli.py
    config.py
    service.py
    loop.py
    events.py
    messages.py
    prompts.py

    sessions/
      __init__.py
      models.py
      store.py
      manager.py
      compaction.py
      summaries.py

    providers/
      __init__.py
      base.py
      openai_responses.py
      codex_responses.py
      codex_auth.py
      stream.py

    tools/
      __init__.py
      registry.py
      schema.py
      runtime.py
      python_tool.py
      shell_tool.py
      file_tool.py
      session_tool.py
      done_tool.py
      truncation.py
      artifacts.py

    browser/
      __init__.py
      daemon.py
      ipc.py
      cdp.py
      chrome.py
      targets.py
      screenshots.py
      downloads.py
      profile.py
      helpers.py

    workspace/
      README.md
      agent_helpers.py
      skills/
        README.md

    tui/
      __init__.py
      app.py
      state.py
      widgets/
        sessions.py
        transcript.py
        browser_status.py
        tool_runs.py
        artifacts.py

    evals/
      __init__.py
      runner.py
      datasets.py
      judge.py
      metrics.py
```

LLM-visible files should be optimized for reading:

- `src/agent/workspace/README.md`: short explanation of where to add helpers.
- `src/agent/workspace/agent_helpers.py`: blank or tiny, explicitly agent-editable.
- `src/agent/browser/helpers.py`: concise, stable helper layer.
- `src/agent/prompts.py`: short browser-specific instructions.
- `src/agent/tools/python_tool.py`: readable contract for the main tool.

Robust internal files can be less "LLM optimized" because the model should rarely need to touch them:

- TUI widgets
- auth refresh internals
- event-store implementation
- CDP daemon internals
- subprocess management

## Core Runtime Concepts

### Session

Session fields:

```text
id
parent_id
title
created_at
updated_at
status: idle | running | waiting | done | failed | aborted
cwd
workspace_dir
browser_profile_id
browser_target_id
model
provider
reasoning_effort
system_prompt_id
compact_summary_id
metadata
```

History should be append-only. Use JSONL for inspectability:

```text
.agent/sessions/<session_id>/
  session.json
  events.jsonl
  messages.jsonl
  tool_calls.jsonl
  artifacts/
  scratch/
```

Do not make SQLite required for the first implementation. JSONL is easy for the agent to inspect, patch, and replay. SQLite can be added later for indexing and large histories.

### Event Stream

Everything writes events:

- session created/updated
- user message
- assistant text delta
- assistant reasoning delta if available
- tool call started
- tool call output chunk
- tool call completed
- artifact created
- browser target changed
- screenshot captured
- compaction started/completed
- background session started/completed
- final answer
- error/warning

The TUI reads events. The agent loop reads/writes session state. The event stream is the integration boundary.

### Agent Loop

Use the clean pi-mono shape:

- `Agent.prompt(session_id, input)`
- `Agent.continue(session_id)`
- `Agent.abort(session_id)`
- `Agent.wait(session_id)`
- an inner loop that runs until final text or `done`
- optional steering/follow-up queues
- hooks before/after tool calls
- model/provider errors are emitted as stream events, not thrown through the whole runtime when recoverable

Use BU's good parts:

- dependency overrides for tools
- typed events
- final `done` semantics
- model retry policy
- compaction hooks
- persisted tool-call history
- output truncation/spillover

Avoid BU's browser-use-specific top layer and special subagent tools.

Use Codex's good parts:

- one running turn per session
- interrupt and pending input behavior
- separate session config from turn context
- event protocol designed for UI replay
- robust process execution and output buffering
- compaction as a task with events

Avoid Codex's heavy coding harness defaults:

- git-specific instructions
- patch-review assumptions
- permission/sandbox UX as the center of the system
- broad skill/plugin machinery as default
- code-review wording in browser tasks

## Model Provider and Codex Auth

### Provider Interface

Provider interface:

```text
stream_response(
  session,
  messages,
  tools,
  model,
  reasoning_effort,
  previous_provider_state,
) -> async stream[ProviderEvent]
```

Provider events should normalize:

- text deltas
- reasoning deltas/summaries
- tool calls
- tool argument deltas if available
- response completed
- usage
- retryable errors
- fatal errors
- provider-specific encrypted reasoning/message items

Store provider-specific replay fields, but keep the core message model provider-neutral.

### Codex Responses

Implement a Codex Responses provider using the patterns from opencode, pi-mono, Codex, Hermes, and OpenClaw.

Expected endpoint:

```text
https://chatgpt.com/backend-api/codex/responses
```

Auth headers:

```text
Authorization: Bearer <access_token>
ChatGPT-Account-Id: <account_id>   # when available
Content-Type: application/json
```

Request shape:

```json
{
  "model": "gpt-5.4",
  "instructions": "...",
  "input": [],
  "tools": [],
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "store": false,
  "stream": true,
  "prompt_cache_key": "<session_id>",
  "reasoning": {"effort": "medium", "summary": "auto"},
  "include": ["reasoning.encrypted_content"]
}
```

Implementation details to port:

- Convert chat-style history to Responses input items.
- Preserve encrypted reasoning items for replay when available.
- Preserve assistant message items/phase where the backend benefits from exact replay.
- Use deterministic fallback tool call IDs to avoid breaking prompt cache.
- Ensure the request input is not empty; OpenClaw notes Codex backend can reject empty input even when instructions exist.
- Support multimodal function-call outputs for screenshots: text plus `input_image` content in the returned tool output.
- Use `prompt_cache_key = session_id`.
- Support streaming over SSE first; WebSocket can be added after core correctness.
- Parse partial tool argument deltas.
- Retry transient connection errors with backoff.
- On 401/403, try token refresh once before failing the turn.
- Add a provider smoke test that calls a fake tool returning a 1x1 PNG as `input_image`. If Codex backend rejects it, enable the fallback synthetic input-item path for attached screenshots.

### Codex OAuth Strategy

There are two viable auth flows:

1. PKCE localhost browser login, like Codex/opencode/pi.
2. Device code login, like Hermes.

The first implementation should support device code login first, then PKCE.

Reasoning:

- Device code is easier inside a terminal UI and remote/SSH sessions.
- Hermes explicitly avoids sharing refresh tokens with Codex CLI or VS Code because refresh tokens can be single-use and cause `refresh_token_reused` failures.
- PKCE is still useful for the smooth local browser flow.

Auth constants and endpoints from the reviewed repos:

```text
client_id: app_EMoamEEZ73f0CkXaXp7hrann
issuer: https://auth.openai.com
token_url: https://auth.openai.com/oauth/token
device start: https://auth.openai.com/api/accounts/deviceauth/usercode
device poll: https://auth.openai.com/api/accounts/deviceauth/token
device verification URL: https://auth.openai.com/codex/device
PKCE callback default port: 1455
PKCE callback fallback port: 1457
```

Credential storage:

```text
~/.llm-browser/auth.json
```

Store:

- provider: `openai-codex`
- auth_mode: `chatgpt`
- access_token
- refresh_token
- account_id when available
- id token claims when available
- last_refresh
- source: `device-code` or `pkce`

Requirements:

- File mode `0600`.
- Cross-process file lock.
- Do not auto-import `~/.codex/auth.json` at runtime.
- Offer one-time explicit import of Codex CLI credentials if found.
- Prefer a harness-owned session to avoid refresh-token races.
- Refresh under lock.
- Mark permanent refresh failures and ask for relogin.
- Redact tokens from logs and TUI.

OpenClaw shows one useful fallback: non-prompting reads of Codex CLI auth can be used for status/availability detection. For this harness, that should remain diagnostic only unless the user explicitly imports.

## Browser Runtime

### CDP Daemon

The browser harness already has the right concept: one daemon owns the CDP websocket and the harness talks to it through IPC.

Implement:

- local Chrome launch with `--remote-debugging-port`
- attach to an existing CDP URL
- optional Browser Use cloud/browser provider attach later
- Unix socket IPC on macOS/Linux, loopback TCP fallback on Windows
- session/name isolation
- stale daemon detection
- stale browser target detection
- automatic reattach on disconnect when safe
- event buffer
- target routing for pages, popups, iframes
- tab list and active tab management
- dialog handling
- download tracking
- screenshot capture
- network event capture
- profile management

Profile default:

- Keep a reusable harness profile template for normal browsing state.
- For each top-level task, copy that template directory into a run/session profile directory.
- Do not point Chrome at the user's real Chrome profile.
- Child sessions may share the copied task profile and even the same tab unless told otherwise.

Expose raw:

```python
cdp("Page.navigate", {"url": "https://example.com"})
cdp("Runtime.evaluate", {"expression": "document.title", "returnByValue": True})
```

Do not hide CDP behind a fragile semantic layer.

### Helper Layer

Ship a compact helper layer:

```python
new_tab(url=None)
tabs()
active_tab()
activate_tab(target_id)
close_tab(target_id=None)
cdp(method, params=None, target_id=None)
js(expression, await_promise=True)
eval_js(expression)
wait_for_load(timeout=10)
wait_for_network_idle(timeout=10)
wait_for_element(selector, timeout=10)
click_at(x, y)
type_text(text)
press(key)
scroll(dx=0, dy=700)
screenshot(name=None, full_page=False, attach=False)
download_info()
upload_file(selector_or_point, path)
```

Important principle: every helper is a convenience, not a boundary. If a helper fails, the model can call raw CDP, JavaScript, shell, or edit the helper itself.

### Visual Observation

Observation can combine multiple channels when requested:

- screenshot artifact
- narrow JS/CDP extraction
- selected accessibility slice
- page URL/title
- active target/tab list
- viewport size
- recent console errors
- recent network failures
- pending downloads

There does not need to be a new high-level `observe()` abstraction in MVP. Use `screenshot(..., attach=True)` for visual state, `js(...)` or raw `cdp(...)` for narrow page extraction, and helper functions only when they prove useful. When the provider supports images in tool results or next-turn attachments, include selected screenshots as image inputs without requiring a separate screenshot-only model turn.

The harness should distinguish three screenshot audiences:

- Model-visible image output: small selected screenshots attached to the tool result so the model can reason over them immediately.
- TUI artifact: every screenshot can be shown in the terminal UI and opened from disk.
- Replay artifact: screenshots are saved with metadata so eval traces can be inspected later.

This keeps the "freedom" model intact: the agent can decide when to attach an image, when to save only, and when to attach multiple images from one browser action batch.

Add a first-class screenshot timeline:

```text
ScreenshotFrame {
  seq
  label
  created_at
  url
  title
  artifact_path
  model_attached: bool
  note
}
```

The timeline is shown in the TUI as it is produced and serialized into the tool result in order. This directly addresses tasks where legitimacy depends on the sequence of visual states, not just the final page.

### Interaction Defaults

The harnesless lesson is important: coordinate-level clicks are often more reliable than DOM-level clicks because they pass through iframes, shadow DOM, cross-origin UI, and canvas. The helper layer should make both paths easy:

- coordinate click via CDP Input domain
- DOM click via JavaScript
- accessibility tree selection
- keyboard navigation
- OS-level fallback through shell/AppleScript

No single strategy should be blessed as the only way.

## Tool Surface

Keep model-visible tools few and powerful.

### `python`

Main browser tool. Runs code in a persistent Python runtime with helpers preloaded.

Requirements:

- persistent globals per session
- CDP helpers preloaded
- artifact helper preloaded
- can import from `workspace/agent_helpers.py`
- supports async CDP under a simple sync wrapper where possible
- timeout and cancellation
- streams stdout/stderr
- spills huge output to artifact file
- returns artifact references
- can emit screenshots as artifacts and model-visible image outputs from the same tool result

This is the central latency reducer. The model can write 50 lines of Python that do a whole browser interaction cycle in one tool call.

The important contract:

```python
screenshot("homepage_loaded", attach=True)
click_at(312, 440)
screenshot("after_clicking_checkout", attach=True)
wait_for_network_idle()
screenshot("after_network_idle", attach=True)
return {"summary": "..."}
```

The runtime converts attached screenshots into ordered multimodal function-call output content for the next provider request, while also saving the files under the session artifacts directory and streaming frame events to the TUI.

### `shell`

General OS escape hatch.

Requirements:

- command, cwd, timeout
- optional PTY
- persistent process ID with `write_stdin`/poll
- streaming output
- output spillover
- cancellation
- environment hygiene

No sandbox approval flow should be central in the initial browser harness. The user explicitly wants freedom. Safety can be a config profile later, not the default conceptual center.

### `files`

Use the opencode/Codex file-editing shape as the baseline, not a new invented file API.

Expose:

- `read(filePath, offset?, limit?)`: file or directory, line windows, binary/image/PDF awareness where useful.
- `glob(pattern, path?)`: rg-backed file discovery, sorted by recency, capped output.
- `grep(pattern, path?, include?)`: rg-backed content search with line numbers and capped output.
- `edit(filePath, oldString, newString, replaceAll?)`: exact string replacement with line-ending/BOM preservation and diff output.
- `write(filePath, content)`: full file write/create with diff output.
- `apply_patch(patchText)`: multi-file patch with streaming patch-update events.

The model needs enough coding power to edit helpers and generated scripts. It does not need a full coding-agent instruction stack.

Implementation details to copy:

- per-file write locks
- absolute-path normalization against session cwd
- helpful "did you mean" suggestions on missing files
- output caps for read/grep/glob
- diff metadata for edit/write/apply_patch
- line-ending and BOM preservation
- post-edit formatter hook where configured
- optional diagnostics hook, but browser harness should keep diagnostics light in MVP

### `session`

Normal session control.

Actions:

- create session
- send prompt
- continue
- abort
- list sessions
- read transcript
- subscribe to events
- inspect status
- switch active browser/profile binding

This replaces special subagent handling.

`wait` should not be implemented as the primary mental model. Waiting for long tasks must be event-driven: callers subscribe to a session event stream, receive status/artifact/tool events, and can cancel or steer while the child is running. A convenience `await_done(session_id, timeout)` can exist for scripts, but the core runtime and TUI should never be a blocking wait loop.

### `done`

Explicit final completion. Keep BU's useful "done tool" pattern because browser eval tasks often need structured final answers and the loop should know when to stop.

## Subagents as Normal Sessions

Adopt opencode's shape:

- A background task creates a real session with `parent_id`.
- It has the same session store, same tools, same browser runtime access policy, same compaction, same artifact system.
- The parent gets a `task_id`/`session_id`.
- The parent can resume the child by sending more input.
- The child can run in the background.
- The TUI can display it just like the main session.
- The parent can subscribe to child session events instead of polling in a loop.
- The parent or user can cancel/steer child sessions at any time.

Session creation should accept:

```text
title
prompt
agent_mode: main | research | browser-worker | extractor | verifier
parent_id
browser_profile_id
browser_target_policy: shared | isolated | no-browser
model
reasoning_effort
```

Important browser-specific policies:

- For independent research/extraction, child sessions can use isolated tabs or profiles.
- For helping the main browser task, child sessions can share read-only artifacts rather than racing the same tab.
- Directly sharing the same active browser target is allowed by default because it preserves browser-harness freedom. Coordination should be done through natural-language instructions and tool locks. A parent can explicitly mark tabs/targets as "do not touch" when needed.

## Compaction

Compaction must be robust from day one.

Borrow from BU:

- threshold-based compaction
- keep recent messages
- summarize task, progress, current state, remaining work, files/artifacts
- preserve tool-call history separately

Borrow from opencode/Codex:

- compaction is itself a task/event
- preserve a recent tail
- cap tool outputs before summarization
- hide old completed compactions
- support manual and automatic compaction
- after compaction, continue the interrupted turn when possible

Browser-specific summary sections:

```text
Goal
User constraints
Current browser state
Important URLs
Tabs and targets
Credentials/auth state notes
Artifacts/screenshots/downloads
Extraction results so far
Failed approaches
Next steps
```

Do not feed every screenshot back forever. Store them as artifacts; keep the latest relevant one and a textual visual summary in context.

## Tool Output and Artifact Storage

Oversized outputs must never destroy a turn.

Rules:

- Keep a preview in the tool result.
- Save full output under the session's artifact directory.
- Return the artifact path and suggested grep/read command.
- Use head+tail truncation for logs.
- Use token-aware truncation when possible.
- Preserve binary artifacts separately.

Artifact types:

- screenshot PNG/JPEG
- full-page screenshot
- optional narrow DOM/accessibility extraction artifact
- accessibility snapshot
- network HAR or NDJSON
- console log
- downloaded file
- generated script
- extraction result JSON/CSV
- huge stdout/stderr
- PDF/text extraction
- trace bundle

## Terminal UI

Build the TUI last enough that the core is real, but early enough that debugging is pleasant.

Current rewrite decision: use Rust/Ratatui. The old Textual/Rich recommendation was for the Python prototype and should not guide this branch.

TUI layout:

```text
left: sessions
  active session
  background sessions
  status: idle/running/waiting/done/error

center: transcript/event stream
  user messages
  assistant text
  tool calls
  tool summaries
  warnings/errors

right: browser/artifacts
  active URL/title
  CDP status
  target list
  latest screenshot thumbnail/path
  downloads
  created artifacts

bottom: composer/status
  prompt input
  model/provider
  active tool/process
  token/context indicator
```

TUI commands:

- `/new`
- `/switch <session>`
- `/sessions`
- `/browser start|stop|status|tabs`
- `/compact`
- `/abort`
- `/artifacts`
- `/eval <dataset> <task_id>`
- `/auth codex login|status|logout`
- `/model`
- `/help`

Do not make the UI a marketing page or a wizard. The first screen should be the actual working session manager.

## Evaluation Harness

Implement an eval runner around the datasets before polishing the TUI.

Input:

```text
dataset path
task ids or range
model/provider
browser profile
max turns
max wall time
artifact dir
```

Outputs:

- final answer
- transcript
- screenshots
- tool count
- LLM call count
- screenshot count
- CDP call count
- token usage
- wall time
- errors/failures
- success verdict, if judge configured

Start with `real_v8.json`, then `real_v14_short.json`.

Use LLM-based self-evaluation early. The evaluator should be a normal session/subagent that reads the trace bundle, screenshots, artifacts, and final answer, then writes a verdict and failure analysis. This keeps evaluation inside the same session/event system instead of building a separate judge framework first.

Eval success should not be binary only. Track failure taxonomy:

- navigation failure
- anti-bot/login/captcha
- missing screenshot/visual observation
- bad extraction
- stale browser target
- CDP disconnect
- download/PDF failure
- context overflow
- model stopped early
- too many tool turns
- final answer formatting mismatch

## Robust vs Dynamic Split

Robust, implemented carefully:

| Area | Why it must be robust |
| --- | --- |
| Codex auth and token refresh | Broken auth kills all runs and can invalidate refresh tokens. |
| CDP daemon and target routing | Browser automation depends on a stable connection. |
| Session/event store | Needed for resume, TUI, subagents, evals, debugging. |
| Tool runtime/cancellation | Stuck Python/shell calls are common in browser tasks. |
| Output truncation/artifacts | Browser tasks generate huge HTML, logs, PDFs, screenshots. |
| Compaction | Long extraction tasks will exceed context. |
| Screenshot/artifact handling | Visual state is central to browser tasks. |
| TUI event subscription | UI should remain reliable while the agent is unstable. |

Dynamic, agent-editable:

| Area | Why it should stay dynamic |
| --- | --- |
| CDP helper recipes | Site behavior varies constantly. |
| Extraction scripts | One-off task structure is unpredictable. |
| Domain skills | Shopping, PDF, maps, social sites, government portals need different heuristics. |
| Browser workarounds | Sometimes AppleScript, curl, or direct APIs are the right answer. |
| Prompt snippets | We will learn better task-specific guidance during evals. |
| Site-specific adapters | Should not pollute the core. |

## Reuse vs From Scratch

Build the project from scratch for conceptual control and small surface area. Reuse ideas and selectively port small substrate modules.

Port or adapt:

- harnesless CDP daemon architecture
- harnesless helper naming and raw `cdp`
- BU event model, tool decorator ideas, dependency overrides
- BU truncation and compaction structure
- opencode normal-session subagents
- opencode tool output spillover
- pi-mono agent loop/steering/follow-up shape
- pi-mono Codex Responses provider details
- Codex unified exec process lifecycle ideas
- Codex event stream/session split
- Codex PKCE auth details
- Hermes device code login and separate-token-store lesson
- OpenClaw non-prompting Codex CLI credential detection and auth profile health ideas

Do not port:

- BU's browser-use-specific tool catalog as the central layer
- BU's special subagent abstraction
- opencode's full coding-agent modes
- Codex's git-heavy prompts and approval/sandbox-first UX
- OpenClaw's multi-platform gateway complexity
- Hermes's huge gateway/session routing stack

## Minimum Necessary Core vs Promptable Behavior

Absolutely necessary in code:

| Need | Why prompting is not enough |
| --- | --- |
| CDP daemon, target routing, stale recovery | The model cannot reliably repair a broken websocket or target registry mid-turn without substrate help. |
| Persistent Python runtime with raw CDP | This is the main action bandwidth mechanism and has to preserve state across tool calls. |
| Ordered screenshot timeline and model-visible images | If images are only file paths, the screenshot-read round trip returns. |
| Event stream/session store | TUI, cancellation, background sessions, replay, and evals all depend on durable events. |
| Minimal internal serialization | Parallelism without serialization around browser/profile/file state creates race bugs. |
| Cancellation/interruption | Long browser actions and subagents need out-of-band cancellation. |
| Output spillover/artifacts | HTML, logs, PDFs, and screenshots exceed context constantly. |
| Codex auth/refresh | Auth failure blocks all real use and can invalidate tokens. |
| File read/grep/glob/edit/write/apply_patch | The model must be able to self-improve helpers safely and quickly. |
| Compaction | Long browser tasks will exceed context. |

Promptable or workspace-level:

| Behavior | Why it can start as prompt/helper |
| --- | --- |
| Site-specific navigation recipes | The model can write these in `agent_helpers.py`. |
| Cookie banner handling | Useful as an editable skill, not core v1 policy. |
| Table extraction heuristics | Start as helper code; promote only if repeated. |
| Direct API discovery patterns | The model can inspect network and write scripts. |
| Human handoff etiquette | Prompt policy is enough initially. |
| Which screenshots to attach | The model should decide using `screenshot(..., attach=True)`. |
| Step-counting conventions | Prompt plus helper can track this. |
| Eval critique style | LLM/subagent judge can start as prompts over traces. |
| Domain-specific browser skills | Keep as optional files, not core. |

## Implementation Phases

### Phase 0: Scaffold and design lock

Deliverables:

- `pyproject.toml`
- `src/agent/*` skeleton
- `src/agent/workspace/agent_helpers.py`
- CLI entrypoint
- config loading
- artifact directory conventions
- initial docs copied from this plan into repo docs

Acceptance:

- `agent --help` works.
- `agent doctor` prints config, data dirs, Python version, Chrome availability.
- Unit tests can import core modules.

### Phase 1: Codex auth and provider

Deliverables:

- `agent auth codex login` with device code.
- `agent auth codex status`.
- token storage with lock and `0600`.
- token refresh.
- Codex Responses streaming provider.
- simple no-tool prompt smoke test.

Acceptance:

- Login succeeds without touching `~/.codex/auth.json`.
- Refresh works under lock.
- A basic prompt streams text.
- A tool-call prompt streams and executes one fake tool.
- 401 triggers one refresh attempt.

### Phase 2: Session store and event bus

Deliverables:

- session create/list/read/update
- append-only events JSONL
- messages JSONL
- tool calls JSONL
- artifact registry
- replay events for TUI/tests

Acceptance:

- Create a session.
- Send a prompt.
- Resume after process restart.
- Inspect transcript/artifacts on disk.

### Phase 3: Browser daemon and CDP helpers

Deliverables:

- start local Chrome with CDP
- attach to existing CDP URL
- daemon IPC
- raw `cdp`
- targets/tabs
- JS eval
- navigate/wait
- screenshot
- click/type/scroll
- event buffer
- stale daemon recovery

Acceptance:

- Start Chrome.
- Open a page.
- Run JS.
- Click/type on a simple local page.
- Capture screenshot.
- Kill/restart daemon and recover.

### Phase 4: Python, shell, and file tools

Deliverables:

- persistent Python tool with CDP helpers
- raw CDP examples in the default workspace guidance
- shell exec with optional PTY and stdin polling
- opencode-style file read/glob/grep/edit/write/apply_patch
- output truncation/spillover
- artifact returns
- ordered screenshot timeline with multiple attached images

Acceptance:

- One Python tool call can navigate, wait, screenshot, and extract text.
- Multiple screenshots from that same Python tool call are visible to the next model continuation in temporal order without an extra screenshot-viewer tool call.
- A long shell process can be polled and stopped.
- Huge output is saved and previewed.
- The model can edit `workspace/agent_helpers.py` and reload it.

### Phase 5: Agent loop

Deliverables:

- tool registry
- prompt construction
- model stream handling
- tool execution
- parallel tool calls where safe
- simple internal serialization for shared browser/file/session state
- hooks before/after tool
- retries/backoff
- final `done`
- user interrupt

Acceptance:

- End-to-end browser task completes on a simple page.
- Tool errors are returned to the model without crashing the runtime.
- Interrupt stops active tool/runtime.
- Empty/incomplete model responses are handled.

### Phase 6: Compaction and artifacts

Deliverables:

- manual compaction
- auto compaction threshold
- browser-specific summary prompt
- screenshot/artifact references in summary
- preserve recent tail
- preserve provider replay items

Acceptance:

- Long synthetic session compacts and continues.
- Tool history survives.
- Large outputs are not inserted into compacted history.

### Phase 7: Sessions as subagents

Deliverables:

- `session` tool create/send/subscribe/abort/list/read/status
- child sessions with `parent_id`
- background runs
- child artifacts linked to parent
- TUI can show parent/child tree

Acceptance:

- Parent starts two background sessions.
- Parent continues working.
- Parent subscribes to child events and reads final outputs without a blocking wait loop.
- Parent or user can cancel/steer a child while it is running.
- Child can be resumed by `session_id`.

### Phase 8: Terminal UI

Deliverables:

- session list
- transcript view
- active tool view
- browser status
- artifact list
- live screenshot timeline
- prompt composer
- slash commands
- auth/model/status commands
- JSON-RPC or WebSocket event bridge modeled after the useful Hermes TUI gateway patterns

Acceptance:

- User can start app, create session, send prompt, watch browser task, switch sessions, inspect artifacts.
- UI remains responsive during long tool calls.
- UI can display streaming screenshots from a running tool and cancel the run.

### Phase 9: Eval runner

Deliverables:

- load `real_v8.json` and `real_v14_short.json`
- run task by id
- collect metrics/artifacts
- LLM judge as a normal evaluator session over the trace bundle
- report output

Acceptance:

- Run at least 5 `real_v8` tasks end to end.
- Run at least 2 `real_v14_short` tasks end to end.
- Produce per-task trace bundles.
- Produce evaluator-session verdicts for each run.

### Phase 10: Hardening and browser-specific extensions

Deliverables:

- download/PDF extraction helpers
- HAR/network request export
- cookie/storage helpers
- file upload/native picker workarounds
- optional Browser Use cloud attach
- optional Chrome extension bridge for things CDP cannot do cleanly
- model/provider fallback
- crash recovery polish

Acceptance:

- Real tasks involving PDFs, dynamic content, pagination, and screenshots are tractable.
- Failure reports are actionable.

## Testing Plan

Unit tests:

- auth store locking and `0600`
- token refresh classification
- Responses message conversion
- provider stream parser
- session store append/replay
- artifact registry
- output truncation/spillover
- ordered screenshot timeline serialization
- compaction summary construction
- tool registry/schema conversion
- internal serialization ordering
- Python runtime timeout/cancel
- shell process polling/cancel
- CDP IPC request/response protocol

Integration tests:

- local Chrome start/stop
- attach to existing Chrome
- navigate/wait/screenshot
- iframe/shadow DOM interaction page
- popup/tab handling
- download handling
- PDF download and text extraction
- long output tool spillover
- restart/resume session
- child session creation/subscription/cancel
- multi-frame screenshot tool result against the real provider path

Eval smoke:

- `real_v8` small subset
- `real_v14_short` small subset
- record call counts and wall time
- compare against previous runs

Manual TUI tests:

- narrow terminal
- long transcript
- multiple background sessions
- streaming screenshot timeline
- active tool streaming output
- auth login flow
- interrupt active run

## Browser-Agent Features Beyond the Initial Spec

The browser agent will likely need:

- request/response capture with body sampling
- automatic URL provenance tracking
- direct fetch/curl helpers from browser cookies
- cookie/localStorage/sessionStorage export/import
- credential-safe profile management
- consent banner heuristics as editable skill
- PDF and document extraction pipeline
- OCR fallback for canvas/image-only pages
- table extraction helpers
- visual diff between screenshots
- viewport/device emulation
- geolocation/timezone/language override
- download folder watcher
- clipboard support
- native file picker fallback
- permission prompt handler
- captcha/human-handoff state
- task budget controls
- failure taxonomy and run report

These should be helpers/skills unless they become substrate-level reliability needs.

## Key Risks

Auth instability:

- Codex subscription endpoints are not a public stable API surface.
- Mitigation: isolate provider code, keep OpenAI API-key provider as fallback, write auth tests, make endpoint/config override easy.

CDP target confusion:

- Chrome exposes fake targets, extension targets, old tabs, popups.
- Mitigation: copy harnesless target filtering lessons, expose `tabs()` clearly, keep raw target IDs visible.

Screenshot token/cost blowup:

- Browser tasks can generate many images.
- Mitigation: artifact all screenshots, attach only selected timeline frames, let the model request `attach=True`, summarize visual state, allow explicit image detail.

Subagent browser races:

- Multiple sessions clicking one tab can corrupt state.
- Mitigation: allow sharing for freedom, but enforce tool locks, show active tab ownership in the TUI, and let the parent mark tabs/targets as do-not-touch in text or metadata.

Overbuilding the TUI:

- A complex UI can delay the harness.
- Mitigation: event stream first, simple TUI second, polish after evals.

Too many helpers:

- A large helper layer recreates the brittle harness problem.
- Mitigation: raw CDP always first-class, helpers are concise and editable, docs explain escape hatches.

## Immediate Next Step After This Plan

Start implementation with Phase 0 and Phase 1:

1. Scaffold `src/agent`.
2. Implement auth storage and Codex device-code login.
3. Implement a minimal Responses streaming provider.
4. Implement session/event JSONL store.
5. Add a fake tool and one real `shell` tool smoke.

Only after that should the browser daemon be wired in, because debugging CDP without a reliable session/provider/event base will be slower.
