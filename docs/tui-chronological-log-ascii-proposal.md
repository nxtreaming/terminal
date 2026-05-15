# TUI Chronological Log ASCII Proposal

This proposal keeps the product grounded in the features the Rust TUI actually
offers today:

- account/auth: Codex login, Claude Code subscription, OpenAI key, Anthropic key,
  OpenRouter key
- model/provider selection via `/model`
- browser backend selection via `/browser`
- browser panel: open live browser, reconnect, change browser
- history via `tab` or `/history`
- Laminar telemetry via `/laminar`
- task controls: start task, follow-up, stop, retry, new task

The core layout rule is:

1. The ready screen is a setup console plus composer.
2. The running/done screen is a chronological transcript.
3. Model/provider/browser/status stay visible, but compact.
4. Thinking is shown as provider-exposed reasoning when available; otherwise use
   a truthful phase label derived from tool/activity events.

## Ready Screen

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

Recent . 28 total
  [x] i turned off caps lock, how can i emulate caps lock?                                46m ago
  [x] https://913524915764-hsgrgohy.us-east-1.console.aws.amazon.com/...                 56m ago
  [x] whats this repo about? explain super high level pls                                22h ago
  [x] Read-only repository exploration: identify what this repo is about...              22h ago

ASK
+----------------------------------------------------------------------------------------------+
| > Tell the browser what to do...                                                              |
+----------------------------------------------------------------------------------------------+
  [ type task ]  [ tab history ]  [ /browser ]  [ /model ]  [ /auth ]
                                                                                              /
```

Notes:

- No fake tabs.
- No task-complete rail.
- The header shows model/account/browser before any task starts.
- History is present but secondary. It should not look like the main surface.
- `cwd` is useful because tasks execute from the current directory.

## First Run Setup

```text
browser-use setup / authenticate ------------------------------------------------------ step 1/3

+----------------------------------------------------------------------------------------------+
| Choose account                                                                               |
|                                                                                              |
|  > Codex login                   uses your ChatGPT plan                                      |
|    Claude Code subscription      uses your Claude Pro/Max                                    |
|    OpenAI API key                bring your own key                                          |
|    Anthropic API key             bring your own key                                          |
|    OpenRouter API key            many models, one key                                        |
+----------------------------------------------------------------------------------------------+

enter select   esc quit
```

```text
browser-use setup / model ------------------------------------------------------------- step 2/3

recommended
  > GPT-5.5                  Codex login             best default                      *
    Claude Sonnet 4.6        Claude Code sub         good browser agent
    Claude Opus 4.7          Claude Code sub         latest, strongest

bring your own key
    GPT-5.5                  OpenAI key              needs key
    Claude Sonnet 4.6        Anthropic key           needs key
    Claude Opus 4.7          Anthropic key           needs key

openrouter
    Qwen3.6 Plus             OpenRouter key          needs key
    Kimi K2.5                OpenRouter key          vision + tools
    DeepSeek V4 Pro          OpenRouter key          needs key

enter select   esc back
```

## Running Transcript

```text
what is this repo about? explain super high level pls

Thinking: Planning repository scan
  I need the repo shape first, then the primary docs and workspace manifest.

  -> list_files .
     Cargo.toml
     README.md
     crates/browser-use-core
     crates/browser-use-store
     crates/browser-use-tui
     python/llm_browser_worker

Thinking: Reading project entry points
  The crate names suggest a Rust runtime, SQLite state, provider adapters, and
  a Ratatui UI. Reading README and Cargo.toml should confirm the architecture.

  -> read_file README.md
  -> read_file Cargo.toml

Thinking: Summarizing architecture
  The central flow is: create a session, send messages to the provider, stream
  model text and tool calls, execute tools, persist events, repeat until done.

Answer draft
  This repo is a Rust-first browser agent workbench. It runs LLM agents that can
  control or inspect browsers, records durable session state in SQLite, and
  exposes a terminal UI built with Ratatui.

GPT-5.5 . Codex . 18s . working                                                   esc stop

ASK
+----------------------------------------------------------------------------------------------+
| > Type to steer the agent...                                                                 |
+----------------------------------------------------------------------------------------------+
  [ esc stop ]  [ f2 browser ]  [ /task new ]
                                                                                              /
```

Notes:

- The user prompt is the first transcript item.
- Thinking appears inline, not in a dashboard.
- Tool calls sit under the thinking block that motivated them.
- Streaming answer is labeled `Answer draft` until completion.
- The bottom status line is a compact footer, not a dashboard.

## Running With Browser Activity

```text
open the partnercentral dashboard and click on partners

Thinking: Preparing browser
  I need a live browser session before interacting with the AWS page.

  -> browser.open_live_view
     Browser Use cloud ready

Browser: partnercentral/dashboard
  title    AWS Partner Central
  page     https://913524915764-hsgrgohy.us-east-1.console.aws.amazon.com/partnercentral/dashboard
  tabs     1 open

Thinking: Finding the Partners entry
  The dashboard is loaded. I will inspect visible navigation labels and select
  the Partners destination.

  -> browser.click text="Partners"

Answer draft
  I opened the dashboard and clicked Partners.

GPT-5.5 . Codex . Browser Use cloud . 56s . working                              esc stop

ASK
+----------------------------------------------------------------------------------------------+
| > Type to steer the agent...                                                                 |
+----------------------------------------------------------------------------------------------+
  [ esc stop ]  [ f2 browser ]  [ /task new ]
                                                                                              /
```

Notes:

- Browser state only becomes prominent when browser activity matters.
- The browser block is still chronological.
- The right mental model is "what happened next", not "dashboard panels".

## Done Transcript

```text
what is this repo about? explain super high level pls

Thinking: Planning repository scan
  -> list_files .

Thinking: Reading project entry points
  -> read_file README.md
  -> read_file Cargo.toml

Thinking: Summarizing architecture

Answer
  This repo is a Rust-first browser agent workbench. It runs LLM agents that can
  control or inspect browsers, records durable session state in SQLite, and
  exposes a terminal UI built with Ratatui.

  Main pieces:
  - browser-use-core: agent loop, provider execution, tools, telemetry
  - browser-use-store: durable SQLite sessions, events, artifacts, settings
  - browser-use-tui: terminal UI
  - python/llm_browser_worker: Python worker for browser helper code

GPT-5.5 . Codex . 31s . done

ASK FOLLOW-UP
+----------------------------------------------------------------------------------------------+
| > Ask a follow-up...                                                                         |
+----------------------------------------------------------------------------------------------+
  [ type follow-up ]  [ tab history ]  [ f2 browser ]  [ /task new ]
                                                                                              /
```

Notes:

- The same transcript remains selectable plain text after completion.
- `Answer draft` becomes `Answer`.
- Follow-up composer stays anchored at the bottom.

## Slash Palette

```text
ASK
+----------------------------------------------------------------------------------------------+
| > /                                                                                          |
+----------------------------------------------------------------------------------------------+
  actions -------------------------------------------------------------------------- esc close
  > filter actions...

  > /task      start a new task
    /history   browse previous tasks
    /browser   change browser backend
    /model     choose model and provider
    /auth      sign in to a provider
    /laminar   configure Laminar telemetry
  --------------------------------------------------------------------------------------------
  up/down navigate . enter select
                                                                                              /
```

Notes:

- This is the right place for command discovery.
- The bottom hint row should not try to teach every command all the time.

## Browser Panel

```text
browser-use / browser ------------------------------------------ Browser Use cloud ready   GPT-5.5

Current browser

  backend    Browser Use cloud
  status     ready
  title      AWS Partner Central
  page       https://913524915764-hsgrgohy.us-east-1.console.aws.amazon.com/partnercentral/dashboard
  live view  available
  tabs       1 open
  viewport   1440x900

  > Open live browser
    Reconnect
    Change browser

enter select   esc back
```

## History Panel

```text
browser-use / previous work ------------------------------------ Browser Use cloud ready   GPT-5.5

  > [x] i turned off caps lock, how can i emulate caps lock?                         46m ago
    [x] https://913524915764-hsgrgohy.us-east-1.console.aws.amazon.com/...          56m ago
    [x] whats this repo about? explain super high level pls                         22h ago
    [x] Read-only repository exploration: identify what this repo is about...       22h ago

enter open   r resume   esc back
```

## Developer / Laminar Panel

```text
browser-use / developer ---------------------------------------- Browser Use cloud ready   GPT-5.5

Laminar

  status     not configured

  > Configure Laminar

Current task

  trace      not available
  events     28 recorded
  tokens     unavailable

Events

   001  session.created          {"task":"what is this repo about?..."}
   002  provider.turn.started    {"model":"gpt-5.5"}
   003  tool.call.started        {"name":"list_files"}
   004  tool.call.finished       {"name":"list_files"}

esc close
```

## Compact Narrow Layout

For narrower terminals, keep the transcript first and compress metadata.

```text
browser-use ------------------------------ GPT-5.5 . Codex . cloud idle

Recent
  [x] whats this repo about? explain super high level pls        22h

ASK
+--------------------------------------------------------------+
| > Tell the browser what to do...                             |
+--------------------------------------------------------------+
  [ type task ]  [ tab history ]  [ / commands ]
                                                             /
```

```text
what is this repo about?

Thinking: Reading project entry points
  -> read_file README.md
  -> read_file Cargo.toml

Answer draft
  This repo is a Rust-first browser agent workbench...

GPT-5.5 . Codex . 18s . working                   esc stop

ASK
+--------------------------------------------------------------+
| > Type to steer the agent...                                 |
+--------------------------------------------------------------+
  [ esc stop ]  [ /task new ]
                                                             /
```

## Implementation Shape

This is intended to be implementable with the current state model:

- Replace the main `work_lines` objective-tree path with transcript-style lines.
- Reuse or adapt the existing `native_plain_transcript_lines`,
  `append_transcript_turns`, `append_turn_activity`, and result block helpers.
- Rename running output from `streaming` to `Answer draft`.
- Keep `ready_lines` as the setup console plus recent work.
- Keep the existing slash palette actions exactly as implemented.
- Keep `tab`, `f2`, `ctrl+e`, `esc`, and slash commands as the real controls.
- Add `cwd` to ready metadata from `current_dir` or the selected session cwd.

## Copy Rules

- Use `Thinking: <specific phase>` for reasoning/phase blocks.
- Use `-> tool_name args` for tool calls.
- Use `Answer draft` while streaming.
- Use `Answer` after completion.
- Use `Browser: <page/title>` only when browser activity occurred.
- Do not show permissions, hooks, or features this TUI does not have.
