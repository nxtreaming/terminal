# Tool-Aware Chronological TUI Proposal

This is the proposal for making the Browser Use TUI feel closer to Grok/Codex:
not by copying exact chrome, but by adopting the same prompt-first transcript
mental model and the same calm visual hierarchy.

This is not a launcher redesign. The task screen is the product. The product
should read like a chronological task feed:

```text
current task prompt
thought / assistant note
semantic event
event output preview
thought / assistant note
semantic event
event output preview
answer / result
fixed composer
```

The current UI makes the user decode runtime internals:

```text
[box] Task complete
AGENT / MODEL / BROWSER / TASK
01 helpers repo-explorer finished / repo-explorer: waiting for gpt-5.5
result
...
```

The Grok-like version should make each step answer:

- why is the agent doing this?
- what tool did it use?
- what did the tool produce?
- what is the answer so far?

## Visual Direction

The target is a Grok-like terminal surface:

- dark, quiet background
- bright current prompt as the visual anchor
- muted metadata
- sparse event rows
- strong accent color only for state and event type
- generous vertical air around the current task
- fixed bottom composer
- footer hints written as `Key:action`

No launcher. No dashboard band. No fake tabs. No progress rail.

The compact configuration card is good on the ready/setup screen because it
answers the important pre-flight questions at a glance: model, account, browser,
cwd, and telemetry. It should not become a persistent dashboard during a task.

Suggested color roles:

```text
Role                 Use
---------------------------------------------------------------------------
background           near-black terminal background
surface              slightly lifted prompt/composer surface
primary text         current prompt, answer, important output
muted text           cwd, old tasks, secondary command output, timestamps
blue accent          prompt chevron, command/event category
purple accent        thought/edit markers
green accent         done, success, additions, verified browser state
red accent           errors, deletions, failed steps
amber accent         warnings, retries, waiting
```

Visual rules:

- The current prompt gets the most visual weight.
- The composer uses the same prompt chevron and surface treatment every time.
- Event markers are small and colored; event text does the explaining.
- Event rows render like `: explore`, `: thought 2.1s`, `: edit file.rs`,
  not as large dashboard headings.
- Completed/old context is dimmed, never boxed.
- Dynamic numbers use tabular spacing so timers/tokens do not jitter.
- Long paths, URLs, commands, and footer hints truncate before wrapping into
  visual noise.
- Wide ASCII boxes are not the implementation target.

## Actual Agent Tools

The browser agent currently registers these tool handlers:

```text
exec_command     run a shell command
write_stdin      send input to a running command
apply_patch      edit files with a patch
read_file        read a local text file
search_files     search files with ripgrep-style queries
list_files       list or fuzzy-filter files
view_image       inspect a local image
update_plan      update an explicit task plan
python           run code in the persistent browser namespace
done             finish with the final user-facing result
spawn_agent      start a helper agent
wait_agent       read/wait for helper status/result
send_input       send instruction to helper and wake it
send_message     queue message for helper
followup_task    queue helper follow-up and wake it
list_agents      list helper sessions
close_agent      cancel/close helper session
```

The UI should not render these names one-for-one in most cases. It should group
them by user meaning.

## Tool Rendering Taxonomy

```text
Tool kind             Raw tools                               Render label
--------------------------------------------------------------------------------
model text            model.stream_delta                       Thought / Answer draft
file exploration      list_files, search_files, read_file       Explore
shell                 exec_command, write_stdin                 Run
file edits            apply_patch                               Edit
browser automation    python + browser events                   Browser
image inspection      view_image                                Image
planning              update_plan                               Plan
helper agents         spawn_agent, wait_agent, send_input,
                      send_message, followup_task,
                      list_agents, close_agent                  Helpers
final answer          done, session.done                        Answer
failures              session.failed, tool.failed,
                      agent.failed                              Error
```

## Ready State

Do not build a big launcher. Ready state should use the compact configuration
card, recent tasks if there is room, and the fixed composer. Configuration
overlays like `/model`, `/browser`, `/auth`, and `/laminar` still exist for
changes, but the current state should be visible before launch.

```text
browser-use ------------------------------------------------ GPT-5.5 . Codex . Browser Use cloud idle

+----------------------------------------------------------------------------------------------+
| Browser Use                                                                                  |
|                                                                                              |
| model      GPT-5.5                                      /model                                |
| account    Codex login                                  /auth                                 |
| browser    Browser Use cloud idle                       /browser                              |
| cwd        ~/Documents/browser-use/experiments/llm-browser-rust-rewrite                      |
| telemetry  Laminar not configured                       /laminar                              |
+----------------------------------------------------------------------------------------------+

: done     i turned off caps lock, how can i emulate caps lock?             46m ago
: done     whats this repo about? explain super high level pls              22h ago
: failed   open the dashboard                                               1d ago

> Tell the browser what to do...

Enter:send | Tab:history | /:commands | Esc:clear
```

## Core Transcript Shape

This is the primary running/done layout.

```text
> what is this repo about?

: thought 1.2s
  I’ll read the top-level docs and manifests first, then cross-check the crate
  layout so the summary reflects what is actually here.

: explore
  List files in .
  Search README* in .

: run
  pwd && rg --files -g 'README*' -g 'Cargo.toml' -g 'crates/**/Cargo.toml'
  /Users/greg/Documents/browser-use/experiments/llm-browser-rust-rewrite
  README.md
  Cargo.toml
  crates/browser-use-core/Cargo.toml
  crates/browser-use-tui/Cargo.toml
  ... +37 lines
  Full output saved to artifact.

: thought 1.8s
  The top level says this is a Rust workspace with a Python test harness around
  it. I’m checking crate manifests next to separate runtime from CLI/TUI/browser
  pieces.

: explore
  Read README.md
  Read Cargo.toml
  List crates

: run
  for f in crates/*/Cargo.toml; do printf '%s\n' "$f"; sed -n '1,80p' "$f"; done
  crates/browser-use-browser/Cargo.toml
  [package]
  ... +140 lines
  Full output saved to artifact.

: thought 1.1s
  The docs frame it as an experimental browser-agent product: Rust owns durable
  product state, Python remains the browser execution island.

: explore
  Read lib.rs, main.rs, worker.py

: answer
  This repo is a Rust-first rewrite of a Browser Use terminal agent workbench.
  Rust handles the durable app/product layer: sessions, SQLite state, append-only
  events, agent loop, provider adapters, subagents, cancellation/resume, datasets,
  and the Ratatui TUI. Python is kept as a small worker island for browser
  execution through CDP/browser-harness helpers.

GPT-5.5 . Codex . 1m 21s . done

> Ask a follow-up...
Enter:reply | Tab:history | F2:browser | /:commands | Esc:clear
```

This is the main target. It should feel closer to Grok visually: large prompt,
small colored event markers, dim metadata, generous spacing, and a stable
composer.

## Rendering Rules By Tool

### `exec_command`

Render as `Run`.

```text
: run
  cargo test -p browser-use-tui
  running 28 tests
  test render::tests::ready_screen_shows_model ... ok
  test render::tests::running_transcript_is_chronological ... ok
  ...
```

Rules:

- First line is the command exactly enough to identify it.
- Output preview is indented under it.
- Truncate long output with a clear continuation line.
- If output spills, show the artifact line the runtime already provides.
- Failed commands remain `Run`, but the status line shows failure:

```text
: run
  cargo test -p browser-use-tui
  failed with exit 101
  error[E0425]: cannot find value `foo` in this scope
```

### `write_stdin`

Render under the existing command session instead of as a separate mysterious
tool.

```text
: run
  python manage.py shell
  session 42 running

Input
  print(User.objects.count())

Output
  128
```

If it cannot be attached to the previous command visually, render as:

```text
: run
  wrote stdin to session 42
  print(User.objects.count())
```

### `list_files`, `search_files`, `read_file`

Render as `Explore`.

```text
: explore
  List files in .
  Search "render_composer" in crates/browser-use-tui/src
  Read crates/browser-use-tui/src/render.rs:720
```

When the result is useful, show a compact output preview:

```text
: explore
  Search "ToolHandlerKind" in crates/browser-use-core/src
  crates/browser-use-core/src/tools/mod.rs:7
  crates/browser-use-core/src/lib.rs:1981
```

Rules:

- Do not say `file.list`; say `List files`.
- Do not collapse all file exploration to `helper finished`.
- Prefer path and line when available.
- Keep repeated reads grouped.

### `apply_patch`

Render as `Edit`.

```text
: edit
  modified crates/browser-use-tui/src/render.rs
  modified crates/browser-use-tui/src/main.rs
  modified scripts/tui-terminal-smoke.py
```

If the patch fails:

```text
: error
  patch failed
  context not found in crates/browser-use-tui/src/render.rs
```

Rules:

- Show changed files, not the full patch by default.
- Show add/modify/delete/move if available from `patch.file_changed`.
- Let a transcript/detail view expose the full patch.

### `python`

This is the browser/workbench power tool, so render it by observed intent.

If it produces browser events, render it as a Browser Action Timeline. Raw
Python is an implementation detail in the main UI.

```text
: browser
  connected  Browser Use cloud
  live view  available

: browser observe
  page      Example Domain
  url       https://example.com
  viewport 1440 x 900
  image     initial-page.png

: browser act
  click     "More information"
  method    click_at_xy(184, 312)
  wait      navigation idle

: browser verify
  page      IANA-managed Reserved Domains
  url       https://www.iana.org/help/example-domains
  image     after-click.png
```

If it produces a screenshot/image artifact:

```text
: browser
  Screenshot saved
  artifact  screenshots/example-home.png
```

If it is non-browser Python, render as `Run Python`.

```text
: run python
  parsed 12 rows from downloaded CSV
```

Rules:

- Do not show a giant Python code blob in the main stream unless the user needs
  code-level debugging.
- Summarize browser actions from browser events and artifacts.
- Render browser work as observe -> act -> verify whenever possible.
- Attach screenshots/images to the browser step they prove.
- Keep the raw Python available in transcript/detail view.

### `view_image`

Render as `Image`.

```text
: image
  /tmp/screenshots/dashboard.png
  1440 x 900 image/png
```

If the model later comments on the image, that comment should appear as the next
`Thought` or `Answer draft` block.

### `update_plan`

Render as `Plan`.

```text
: plan
  [x] Inspect current renderer
  [>] Replace objective tree with transcript
  [ ] Run terminal UI verification
```

Rules:

- Only show plan when explicitly updated.
- Do not turn every task into a plan dashboard.

### Helper Agent Tools

Tools:

```text
spawn_agent
wait_agent
send_input
send_message
followup_task
list_agents
close_agent
```

Render as `Helpers`.

```text
: helpers
  started repo-explorer helper
  repo-explorer working

: thought 0.8s
  I’m using a helper to inspect the repo while I check the main entry points.

: helpers
  repo-explorer finished
  result: Rust-first browser agent workbench with TUI, core, store, providers...
```

Rules:

- The helper result should be visible if it contributes to the answer.
- Do not render `repo-explorer: waiting for gpt-5.5` as the main story.
- Helper status is secondary to the user-facing activity.
- If a helper is still running, show it compactly:

```text
: helpers
  repo-explorer working
  waiting for GPT-5.5
```

### `done`

Render as `Answer`.

```text
: answer
  This repo is a Rust-first rewrite of a Browser Use terminal agent workbench.
```

Rules:

- While still streaming, label it `Answer draft`.
- When the `done` tool/session.done fires, label it `Answer`.
- The answer remains selectable plain text.

## Thought / Assistant Text

The UI should display what the model is actually allowed to expose.

Sources:

- provider-visible text deltas from `model.stream_delta`
- explicit assistant text before tool calls
- truthful phase summaries derived from activity events when no text is emitted
- provider-exposed reasoning/thinking deltas, once the protocol records them

Do not invent hidden chain-of-thought. The label can still be pleasant:

```text
: thought 1.4s
  I’ll read the repo’s top-level docs first, then cross-check the crate layout.
```

If there is no assistant text, use a phase label:

```text
: thought
  Waiting for GPT-5.5
```

For retries/errors:

```text
: thought
  Model request hit a transient error. Retrying 1/5.
```

## Streaming Thought

Yes, thought text should stream, but it needs to be a first-class stream separate
from final answer text.

Today the protocol has:

```text
ModelEvent::TextDelta  -> model.stream_delta -> Answer draft / final assistant text
ModelEvent::ToolCall   -> model.tool_call    -> tool execution
ModelEvent::Usage      -> model.usage        -> telemetry/cost
ModelEvent::Done       -> turn finished
```

It should become:

```text
ModelEvent::ThinkingDelta { text, label? }
  -> model.thinking_delta
  -> visible `: thought <duration>` or `: thought <label>` block, streamed live

ModelEvent::TextDelta { text }
  -> model.stream_delta
  -> visible `Answer draft` block, streamed live
```

The UI then renders live reasoning like opencode:

```text
> what is this code about

: thought inspecting code context
  I need to answer the question about the currently opened file. First, I should
  inspect the file using a read function.

  -> Read crates/browser-use-core/src/lib.rs [offset=1, limit=2000]

: thought evaluating file content
  I need to explain the file, but I should check the rest of the module before
  summarizing the architecture.

  -> Read crates/browser-use-core/src/lib.rs [offset=1498, limit=2000]
  -> Read crates/browser-use-core/src/lib.rs [offset=3059, limit=2000]

: thought summarizing key elements
  I’m organizing the details around provider setup, session management, tool
  dispatch, retries, and persisted event history.

Answer draft
  This file, crates/browser-use-core/src/lib.rs, is the core runtime for the
  Rust browser-use agent.
```

Rendering rules:

- `Thought` streams above the tool call it leads to.
- `Answer draft` streams separately from `Thought`.
- If the provider gives a title/summary for the reasoning block, use it:
  `: thought inspecting code context`.
- If the provider only gives raw exposed thinking text, derive a short label
  from the first sentence or current tool intent.
- If the provider gives no reasoning stream, fall back to truthful phase labels:
  `: thought waiting for GPT-5.5`, `: thought retrying model request`, etc.
- When a tool call starts, freeze the current thinking block and append the tool
  action under it.
- Do not mix final answer tokens into the thinking block.

Provider work:

- Codex/OpenAI Responses: extend the SSE parser to recognize reasoning or
  reasoning-summary stream events in addition to `response.output_text.delta`.
  Store them as `model.thinking_delta`.
- Anthropic/Claude: parse exposed `thinking` blocks when returned by Messages.
  If streaming is enabled later, map thinking deltas to the same
  `ModelEvent::ThinkingDelta`.
- OpenRouter/OpenAI-compatible chat: many models will not expose reasoning
  safely. Use fallback phase labels there.

Data model work:

- Add `ThinkingDelta` to `ModelEvent`.
- Add `thinking_text: Option<String>` or richer `thinking_blocks` to
  `TranscriptTurn`.
- Add `turn_thinking_text_from_events` beside `turn_streaming_text_from_events`.
- Keep thinking deltas in event history so completed tasks replay the same
  chronological log.
- Add a setting later if needed: show thinking summaries, hide thinking, or show
  detailed exposed reasoning.

## Browser-Specific Transcript

For browser tasks, the Python tool plus browser events should become a
chronological browser log using the same small event-marker grammar.

```text
> open the AWS Partner Central dashboard and click Partners

: thought 1.0s
  I need a browser session first, then I’ll inspect the navigation and click the
  Partners entry.

: browser observe
  connected Browser Use cloud
  backend   Browser Use cloud
  page      about:blank

: browser observe
  opened   https://913524915764-hsgrgohy.us-east-1.console.aws.amazon.com/...
  title    AWS Partner Central
  tabs     1 open

: browser act
  click    "Partners"

: answer
  I opened the dashboard and clicked Partners.

GPT-5.5 . Codex . Browser Use cloud . 56s . done
```

Browser state should be prominent only when browser activity happened.

## Chosen Browser Rendering: Browser Action Timeline

This is the single browser rendering direction for implementation.

Python browser work should not primarily render as `python`. It should render as
what happened in the browser:

```text
observe -> act -> verify
```

Example:

```text
> open the dashboard and click Partners

: thought 1.0s
  I’ll open the dashboard, inspect the visible navigation, click Partners, then
  verify that the page changed.

: browser
  connected  Browser Use cloud
  live view  available

: browser observe
  page      AWS Partner Central
  url       .../partnercentral/dashboard
  viewport 1440 x 900
  image     initial-dashboard.png

: browser act
  click     "Partners"
  method    click_at_xy(164, 286)
  wait      navigation idle

: browser verify
  page      Partners
  url       .../partnercentral/partners
  image     partners-page.png

: answer
  I opened the dashboard and clicked Partners.
```

Extraction tasks use the same timeline:

```text
: browser observe
  page      Search results
  url       example.com/search?q=laptops

: browser extract
  found     24 product cards
  fields    title, price, rating, url

: browser paginate
  page      2 / 5
```

Form tasks:

```text
: browser fill
  field     email
  field     password

: browser submit
  click     "Sign in"

: browser verify
  page      Dashboard
  image     signed-in-dashboard.png
```

Implementation mapping:

- `browser.live_url` creates or updates the connection block.
- `browser.page` / `browser.state` creates observe or verify steps when title,
  URL, tab count, or viewport changes.
- `tool.image` and screenshot artifacts attach to the nearest browser step.
- Python output can be summarized as act/extract/fill/submit/paginate only when
  the code or emitted events explicitly reveal intent.
- If intent is not explicit, render the truthful neutral state:
  `: browser updated`.
- Raw Python code stays available in a detail/transcript view, referenced as
  `details python tool call <n>` if needed.
- If a Python call emits no browser events, render it as `Run Python`.

## Failed Task

```text
> open the dashboard

: browser
  Connecting Local Chrome

: error
  Could not connect to Local Chrome.

Next actions
  > Reconnect browser
    Change browser backend
    Retry task
    New task

GPT-5.5 . Codex . failed
```

Failure should stay in the same transcript structure. No separate dashboard.

## Empty / No History

```text
browser-use ------------------------------------------------ GPT-5.5 . Codex . Local Chrome idle

+----------------------------------------------------------------------------------------------+
| Browser Use                                                                                  |
|                                                                                              |
| model      GPT-5.5                                      /model                                |
| account    Codex login                                  /auth                                 |
| browser    Local Chrome idle                            /browser                              |
| cwd        ~/Documents/browser-use/experiments/llm-browser-rust-rewrite                      |
| telemetry  Laminar not configured                       /laminar                              |
+----------------------------------------------------------------------------------------------+

No previous work yet.

> Tell the browser what to do...

Enter:send | /:commands | Esc:clear
```

## Persistent Footer

The footer should be one compact line of key-action hints, not a progress
dashboard and not bracketed pseudo-buttons.

Ready:

```text
Enter:send | Tab:history | /:commands | Esc:clear
```

Running:

```text
Esc:stop | F2:browser | /:commands
```

Done:

```text
Enter:reply | Tab:history | F2:browser | /:commands | Esc:clear
```

Failed:

```text
Enter:action | F2:browser | /:commands | Esc:clear
```

## What To Remove From Task Screens

Remove from running/done task screens:

- `[box] Active objective`
- `[box] Task complete`
- `AGENT / MODEL / BROWSER / TASK` dashboard band
- grouped `01 helpers ...` as the primary activity
- `result` as an isolated section detached from the event history
- fake mode/progress rails

The model, account, browser, elapsed time, and state still exist, but only as a
footer/header metadata line.

## Implementation Plan

The current code already has useful primitives:

- `TranscriptTurn`
- `transcript_from_events`
- `native_plain_transcript_lines`
- `append_transcript_turns`
- `append_turn_activity`
- `append_activity_blocks`
- `append_result_block`
- event projections like `activity_from_events`

The implementation should:

1. Make `work_lines` use the transcript renderer as the primary main view.
2. Replace grouped buckets with chronological event blocks.
3. Preserve provider text deltas as `Thought` or `Answer draft`.
4. Add a tool-aware formatter for raw tool/activity events.
5. Render helper-agent activity as `Helpers`, but surface helper result summaries.
6. Render browser/Python events as the Browser Action Timeline when browser
   state/artifacts exist.
7. Apply the Grok-like visual direction: prompt-first task surface, small event
   markers, dim metadata, fixed composer, and `Key:action` footer hints.
8. Rename `streaming` to `Answer draft`; rename final `result` to `Answer`.
9. Keep setup/model/browser/history/developer overlays as separate surfaces.
10. Keep slash palette exactly aligned with real commands:
   `/task`, `/history`, `/browser`, `/model`, `/auth`, `/laminar`.
11. Add browser event grouping for observe/act/verify from browser events,
    screenshot artifacts, and Python call summaries.
12. Verify with `scripts/verify-terminal-ui.sh` after code changes.

## Acceptance Criteria

A user looking at a completed task should be able to answer these in under five
seconds:

- What did I ask?
- What did the model decide to do first?
- Which files/pages/commands did it inspect?
- Did it use a helper?
- Did it use the browser?
- What answer did it produce?
- Which model/account/browser was used?

The current UI fails because the story is split across dashboard regions. The
new UI should make the story the interface.
