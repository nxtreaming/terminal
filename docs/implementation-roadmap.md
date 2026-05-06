# Browser Agent Harness Roadmap

Status: execution plan with the vertical MVP implemented and the main infrastructure slices actively hardened.

This roadmap splits the work into a small vertical MVP and later infrastructure. The MVP is not a toy; it is the thinnest end-to-end product that proves the key idea:

```text
LLM session -> tool execution -> raw CDP/Python browser control -> multiple screenshots returned as model-visible images -> model continues -> task finishes
```

Everything else should be added after that spine works on real browser tasks.

## Current Implementation Snapshot

Implemented:

- durable session/event logs and event bus
- fake, OpenAI Responses, and Codex Responses providers
- harness-owned Codex auth store, Codex CLI import/fallback, device-code login, refresh, logout, and redacted status
- owned Chromium/headless Chromium, Browser Use cloud, explicit CDP, and real Chrome attachment modes
- raw CDP browser runtime with page target/session handling, screenshot artifacts, element crops, download directory configuration, console/network/download helpers, and browser trace JSON export
- persistent Python browser tool with raw `cdp(...)`, editable helper module imports, ordered model-visible image timelines, artifact upload/download helpers, PDF/text/search helpers, and output spillover
- shell tool with streaming, cancellation, long-running process polling/stdin/stop, and optional PTY mode
- file tools with range reads, binary detection, exact edit diagnostics, BOM/newline preservation, diffs, glob, grep, write, and patch
- background child sessions exposed as normal sessions via a model-visible session tool
- cancellation, resume, compaction, trace export, and self-eval child sessions
- Textual TUI with multi-session table, event log, artifact table/preview, trace/eval/resume/cancel/open/report/browser/auth/config commands
- JSON config defaults for provider, model, browser backend/profile/cloud/viewport settings
- dataset sample/run/report commands for `real_v8` and `real_v14_short`

Still intentionally pending or incomplete:

- a separate cross-process browser daemon and IPC layer like the original browser harness
- external sidecar viewer/websocket
- shell completions and installer polish
- broad dataset iteration/regression work after the infrastructure settles
- named profile migration UI and richer profile picker
- live terminal image rendering beyond artifact preview/open

## Definition of Done

### Vertical MVP done

The MVP is done when this command can run a real browser task end to end:

```bash
llm-browser run "Open example.com, inspect the page, capture screenshots before and after your actions, and tell me what you found."
```

The agent must be able to:

- authenticate with one working OpenAI/Codex-compatible provider path
- start or attach to Chrome through CDP using a copied non-default profile
- run a persistent Python browser tool with raw `cdp(...)` access
- execute multi-step browser code in one tool call
- emit multiple ordered screenshots from that one tool call
- send those screenshots back to the model as image inputs on the next continuation
- save the same screenshots as artifacts for replay/TUI
- read and edit local files
- run shell commands
- stop, cancel, and resume a session at a basic level
- display a minimal live terminal view of session events

### Full harness done

The full harness is done when it feels like opencode plus browser harness:

- durable session tree with background sessions/subagents
- event replay, cancellation, steering, and trace inspection
- robust compaction
- opencode/Codex-grade file tools
- hardened browser daemon/profile/download lifecycle
- eval runner over `real_v8` and `real_v14_short`
- LLM self-eval over trace bundles
- provider abstraction, packaging, config, and install flow
- TUI good enough to manage several sessions at once

## Non-Negotiables

- Raw CDP is first-class. Helpers are convenience, not the boundary.
- The model can edit and reload helper code during the task.
- Do not attach whole DOM trees by default.
- Do not create a semantic browser framework as the central API.
- Do not expose complicated scheduling primitives in the MVP.
- Ordered browser actions should usually live inside one Python/browser call.
- Browser mutations are serialized per session/target/profile internally.
- Long tasks and subagents are event-driven, not blocking wait loops.

## Slice 0: Repository Baseline

Status: done.

MVP work:

- Initialize git on `main`.
- Commit the planning baseline.
- Add basic ignores so local machine files do not pollute status.

Later work:

- Add release tags and CI once there is code to validate.
- Add pre-commit formatting only if it helps the repo stay quiet.

Acceptance:

- `git status` is clean before implementation begins.

## Slice 1: Project Skeleton and Event Spine

Purpose: create the smallest runtime that can persist and stream what happens.

MVP work:

- Create the package layout inspired by `packages/bu`, but without BU concepts:
  - `llm_browser/agent`
  - `llm_browser/session`
  - `llm_browser/tool`
  - `llm_browser/browser`
  - `llm_browser/provider`
  - `llm_browser/tui`
  - `llm_browser/workspace`
- Define event types:
  - `session.created`
  - `session.input`
  - `model.delta`
  - `tool.started`
  - `tool.output`
  - `tool.image`
  - `tool.finished`
  - `tool.failed`
  - `session.cancelled`
  - `session.done`
- Store events as append-only JSONL under `.llm-browser/sessions/<session_id>/events.jsonl`.
- Build an in-process event bus with subscription.
- Build a minimal session object:
  - id
  - parent id
  - working directory
  - status
  - created/updated timestamps
  - artifact directory

Later work:

- Add indexed event storage for fast trace browsing.
- Add event schema versioning/migrations.
- Add cross-process event subscribers.
- Add rich trace export/import.
- Add durable session metadata database if JSONL becomes too slow.

What is deliberately missing in MVP:

- No distributed event store.
- No full trace query language.
- No polished replay viewer.

Acceptance:

- A fake session can emit events, persist them, reload them, and stream them to a subscriber.

## Slice 2: Provider and Codex Auth Path

Purpose: get one model call loop working before building a tool universe.

MVP work:

- Implement one provider interface:
  - `start_turn(session, messages, tools)`
  - stream text deltas
  - stream tool calls
  - accept tool outputs
- Implement OpenAI/Codex auth using the learnings from Codex, pi-mono, Hermes, and OpenClaw.
- Prefer Codex subscription auth when available.
- Fall back to explicit API key only if configured.
- Store credentials under this harness's own config directory, not by mutating another tool's files.
- Smoke-test whether image content is accepted inside function-call output for the chosen backend.
- If image-in-tool-output fails, implement the fallback:
  - tool output returns text metadata
  - harness immediately injects synthetic image input items before the next model continuation

Later work:

- Add multiple providers and model routing.
- Add credential pool support.
- Add provider health checks.
- Add provider-specific capability discovery.
- Add retry/backoff rules based on provider errors.
- Add pricing/token accounting.

What is deliberately missing in MVP:

- No generic provider plugin system.
- No multi-model routing.
- No automatic model selection.

Acceptance:

- The harness can run one model turn, receive a tool call, execute a fake tool, return output, and continue.
- The image-return behavior is known by actual smoke test, not assumed.

## Slice 3: Browser Runtime and CDP Daemon

Purpose: give the model a real Chrome it can control freely.

MVP work:

- Start Chrome with:
  - remote debugging enabled
  - copied non-default `--user-data-dir`
  - stable viewport defaults
  - download directory inside the session artifact directory
- Attach to an existing CDP websocket if explicitly configured.
- Maintain one browser daemon per top-level run.
- Expose raw CDP:
  - `cdp(method, params=None, session_id=None)`
  - target listing
  - target attach/detach
  - active page selection
- Copy harnesless-style helpers:
  - `new_tab(url)`
  - `js(expression)`
  - `wait_for_load()`
  - `screenshot(label, attach=True)`
  - `click_at(x, y)`
  - `type_text(text)`
  - `press(key)`
  - `scroll(dx=0, dy=...)`
- Keep helpers in an editable file loaded into the Python tool.
- The first prompt/examples should show raw CDP calls, not only helpers.

Later work:

- Harden daemon recovery:
  - stale websocket detection
  - Chrome crash restart
  - target reattachment
  - orphan process cleanup
- Add better profile templates:
  - logged-in template copy
  - clean template copy
  - per-site persistent template
- Add download tracking.
- Add permission prompt handling.
- Add native dialog/AppleScript fallbacks as editable recipes.
- Add network/console tracing helpers.

What is deliberately missing in MVP:

- No attempt to use the user's live main Chrome profile.
- No whole-DOM observation feed.
- No high-level selector automation framework.

Acceptance:

- A Python snippet can navigate to a site, run raw `Runtime.evaluate`, click via `Input.dispatchMouseEvent`, and capture a screenshot.

## Slice 4: Tool Runtime

Purpose: expose a small set of powerful tools, not a huge browser framework.

MVP work:

- Implement `python` as the main browser tool:
  - persistent namespace per session
  - CDP/browser helpers preloaded
  - artifact helpers preloaded
  - stdout/stderr capture
  - timeout
  - cancellation
  - structured return value
- Implement `shell`:
  - working-directory aware
  - streaming output
  - timeout
  - cancellation
  - large output spillover to file
- Implement basic file tools:
  - `read`
  - `grep`
  - `glob`
  - `edit`
  - `write`
  - `apply_patch`
- Implement `done`.
- Serialize:
  - Python/browser action per session
  - file writes per path
  - browser target/profile mutations

Later work:

- Bring file editing closer to opencode/Codex:
  - exact-string replacement diagnostics
  - line-ending/BOM preservation
  - patch preview
  - better conflict messages
  - range reads
  - binary file detection
  - permission-aware writes
- Add tool permissions/policies only if needed.
- Add long-running command process management.
- Add terminal PTY sessions.
- Add reusable tool plugins, but keep them optional.

What is deliberately missing in MVP:

- No large catalog of semantic browser tools.
- No model-facing lock/barrier DSL.
- No complex parallel tool scheduler.

Acceptance:

- One Python tool call can perform an ordered mini-plan and return text plus multiple images.
- Shell and file tools are good enough for the model to edit helper code and retry.

## Slice 5: Screenshot Timeline and Artifact System

Purpose: solve the screenshot-read problem directly.

MVP work:

- `screenshot(label, attach=True)` captures PNG bytes and saves them as artifacts.
- Each attached screenshot produces:
  - label
  - timestamp
  - artifact path
  - URL
  - title
  - viewport
  - order index
- Tool calls can emit multiple `tool.image` events while running.
- The final tool output includes an ordered image timeline.
- The provider layer sends those images to the next model continuation.
- The TUI displays image events as they arrive, even before the model continues.

Later work:

- Add optional detail level controls:
  - low/auto/high
  - crop
  - full page
  - element crop from JS-provided bounding boxes
- Add image deduplication.
- Add video/GIF trace generation from screenshot timelines.
- Add visual diff helpers.
- Add OCR helpers as optional dynamic code, not always-on context.

What is deliberately missing in MVP:

- No separate screenshot-reader tool.
- No automatic screenshot after every browser action unless the model or helper asks for it.
- No whole-page visual history stuffed into context forever.

Acceptance:

- A single tool call can emit at least three screenshots, and the next model turn can see all three in order.

## Slice 6: Minimal TUI

Purpose: make the system understandable while it runs.

MVP work:

- Implement a terminal app that reads the event stream and shows:
  - current session
  - model text stream
  - running tool
  - tool stdout/stderr summary
  - screenshot timeline labels/artifact paths
  - status/errors
- Add commands:
  - start session
  - send message
  - cancel
  - resume
  - open artifact path
  - quit
- Borrow Hermes's split:
  - core runtime emits events
  - TUI is a client over those events
  - slow handlers do not block UI rendering

Later work:

- Add multi-session panes.
- Add child session/subagent tree.
- Add live screenshot preview protocol if terminal supports it.
- Add trace search.
- Add tool approval/interrupt controls.
- Add config/profile picker.
- Add sidecar websocket for external viewers.

What is deliberately missing in MVP:

- No full opencode-grade UI.
- No complex layout.
- No assumption that the TUI owns agent state.

Acceptance:

- A user can run a browser task, watch tool/screenshot events appear, cancel the run, and inspect saved artifacts.

## Slice 7: Basic Sessions, Resume, and Cancellation

Purpose: make long tasks controllable without building full orchestration first.

MVP work:

- Sessions have statuses:
  - `idle`
  - `running`
  - `cancelling`
  - `cancelled`
  - `done`
  - `failed`
- Cancellation propagates to:
  - provider stream
  - running Python tool
  - shell process
  - browser waits
- Resume loads event history and continues from the last coherent point.
- Parent id exists in session metadata even if full subagent UI is later.

Later work:

- Full opencode-style background sessions.
- Subagents as normal sessions with parent id.
- Session steering while child session runs.
- Event-driven wait/subscription APIs.
- Concurrent session resource accounting.
- Session snapshots and branch/fork.

What is deliberately missing in MVP:

- No special subagent abstraction.
- No background session scheduler beyond one active run and basic parent id.
- No complex resume after arbitrary partial provider/tool failure.

Acceptance:

- Cancelling a running browser/Python action stops it and records a coherent event trail.
- A completed session can be reopened and inspected.

## Slice 8: Compaction and Output Spillover

Purpose: prevent long browser tasks from drowning the context.

MVP work:

- Spill large tool outputs to artifact files.
- Return compact summaries with artifact references.
- Keep screenshots attached only when explicitly marked `attach=True`.
- Add a simple compaction trigger by token/output size.
- Compact by summarizing:
  - user goal
  - important decisions
  - browser state
  - useful artifact paths
  - pending next steps
- Never compact away the event log or artifacts.

Later work:

- Model-assisted compaction with structured state.
- Per-tool truncation policies.
- Trace-aware compaction that keeps relevant screenshots.
- Rehydration of old artifacts/images when the model asks.
- Compaction tests over real task traces.

What is deliberately missing in MVP:

- No elaborate memory system.
- No automatic DOM/state snapshot injection.
- No semantic retrieval over traces.

Acceptance:

- A large shell/Python output is saved to file and the model receives a useful compact reference.

## Slice 9: Dataset Smoke Eval

Purpose: prove the MVP can do real browser tasks, not just example.com.

MVP work:

- Add a tiny eval runner that can execute selected tasks from:
  - `datasets/real_v8.json`
  - `datasets/real_v14_short.json`
- Store each run as a normal session trace.
- Start with manual/human review of traces.
- Add an LLM self-eval prompt as a normal evaluator session, not a special framework.

Later work:

- Batch runner.
- Retry policy.
- Score aggregation.
- LLM judge calibration.
- Regression dashboards.
- Failure clustering.
- Prompt/helper evolution from eval results.

What is deliberately missing in MVP:

- No benchmark platform.
- No hard claim of solved tasks without trace review.
- No special evaluator engine.

Acceptance:

- The harness can run at least a few `real_v8` tasks and save complete traces with screenshots.

## Slice 10: Packaging, Config, and Install

Purpose: make it runnable by the user without remembering internals.

MVP work:

- Add a single CLI entry point:
  - `llm-browser run "..."`
  - `llm-browser tui`
  - `llm-browser auth login`
  - `llm-browser sessions list`
  - `llm-browser sessions resume <id>`
- Add config file:
  - provider/auth mode
  - Chrome path
  - profile template path
  - artifact root
  - default model
- Add local README quickstart.

Later work:

- Installer script.
- Shell completions.
- Config migration.
- Multiple named browser profiles.
- Remote runner mode.
- External viewer integration.

What is deliberately missing in MVP:

- No polished distribution.
- No hosted service.
- No multi-user config.

Acceptance:

- A clean checkout can be configured and can launch the MVP demo command.

## Cross-Cutting Split by Component

| Component | MVP | Later |
| --- | --- | --- |
| Agent loop | One session, streaming model/tool loop, durable events | Multi-session orchestration, steering, branching |
| Provider | One Codex/OpenAI path | Provider plugins, routing, health checks |
| Auth | Codex subscription auth plus API-key fallback | Credential pools, refresh hardening, account management |
| Browser | Start/attach Chrome, copied profile, raw CDP | Crash recovery, profile templates, downloads, permissions |
| CDP | `cdp(...)` plus tiny helpers | Network tracing, console tracing, target recovery |
| Python tool | Persistent browser REPL, artifacts, screenshots | Rich process isolation, dependency envs, tool libraries |
| Shell tool | Basic command exec, streaming, timeout | PTY, long-lived processes, command sessions |
| File tools | Basic read/search/edit/write/patch | Full opencode/Codex editing ergonomics |
| Screenshots | Multi-image ordered timeline in one tool result | Crops, diffs, video, dedup, rehydration |
| TUI | Live event view and commands | Multi-session UI, trace explorer, richer previews |
| Subagents | Parent id exists; no special abstraction | Background sessions as normal child sessions |
| Compaction | Simple summary and spillover | Trace-aware model compaction |
| Eval | Manual smoke runs plus LLM self-eval session | Batch evaluation and dashboards |
| Packaging | Local CLI and config | Installer, completions, migrations |
| Tests | Unit tests for spine plus browser smoke | Broad integration/regression matrix |

## What Must Be Robust From Day One

These pieces are infrastructure even in the MVP:

- event append and replay
- provider/tool streaming boundaries
- cancellation propagation
- screenshot artifact creation
- image attachment into the next model continuation
- Chrome process/profile ownership
- raw CDP request/response matching
- large output spillover
- basic file-write safety

If any of these are flimsy, browser tasks will be painful immediately.

## What Can Stay Prompted or Dynamic

These should not be hardcoded early:

- click/navigation strategies
- selector heuristics
- DOM extraction shape
- cookie consent handling
- table parsing
- app-specific workflows
- OCR
- AppleScript or OS-level workarounds
- direct network/API request strategies
- page-specific helper functions

The model should write these in the persistent Python namespace or editable helper files as needed.

## Implementation Order

1. Project skeleton and event spine.
2. Provider loop with a fake tool.
3. Codex/OpenAI auth and streaming smoke test.
4. Browser daemon with raw CDP and copied profile.
5. Persistent Python browser tool.
6. Screenshot timeline returned to model.
7. Shell and basic file tools.
8. Minimal TUI over events.
9. Cancellation/resume.
10. Output spillover and simple compaction.
11. Dataset smoke runner.
12. README quickstart.

Each item should land as one focused commit unless it is too large, in which case split by runtime/provider/browser/TUI boundaries.

## First Real Demo

The first demo should be intentionally boring:

```python
new_tab("https://example.com")
wait_for_load()
screenshot("loaded", attach=True)

title = js("document.title")
text = js("document.body.innerText.slice(0, 1000)")

return {"title": title, "text": text}
```

The second demo should prove temporal screenshots:

```python
new_tab("https://example.com")
wait_for_load()
screenshot("loaded", attach=True)
cdp("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": 100, "y": 100})
screenshot("after_mouse_move", attach=True)
return {"ok": True}
```

The third demo should use one dataset task from `real_v8.json` and save a full trace.
