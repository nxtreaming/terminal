# Terminal UI Redesign Proposal

## CURRENT DESIGN

The current Rust TUI is a full-screen alternate-screen app with one main workbench and a set of modal overlays.

```text
Terminal
+----------------------------------------------------------------------------------+
| App runtime                                                                       |
| - raw mode                                                                        |
| - alternate screen                                                                |
| - bracketed paste                                                                 |
| - enhanced keyboard reporting                                                     |
| - key events only                                                                 |
|                                                                                  |
| State                                                                            |
| - SQLite store                                                                    |
| - sessions                                                                        |
| - append-only events                                                              |
| - artifacts                                                                       |
| - app settings                                                                    |
| - selected_session_id                                                             |
| - selected overlay row                                                            |
| - composer input/cursor/kill buffer                                               |
|                                                                                  |
| Projection                                                                        |
| - Store sessions + events -> WorkbenchState                                       |
| - WorkbenchState -> current task/result/failure/activity/browser/history          |
|                                                                                  |
| Render                                                                            |
| - first-run setup                                                                 |
| - workbench                                                                       |
| - composer                                                                        |
| - footer                                                                          |
| - overlays                                                                        |
+----------------------------------------------------------------------------------+
```

Current state tree:

```text
App
|-- Workbench
|   |-- Ready
|   |-- Running
|   |-- Result
|   |-- Failed
|   `-- Cancelled
|
|-- Composer
|   |-- single or multiline input
|   |-- up to 10 visible lines
|   |-- custom cursor rendering
|   |-- custom readline-like key handling
|   `-- no real terminal text cursor
|
`-- Overlays
    |-- Setup
    |-- Account
    |-- Model
    |-- Browser
    |-- BrowserChoice
    |-- SetupComplete
    |-- History
    |-- Actions
    |-- Help
    `-- Developer
```

Current first-run setup:

```text
+--------------------------------------------------------------------------------+
|                                                                                |
|               +--browser-use------------------------------------------------+  |
|               | Set up the browser agent                                    |  |
|               |                                                             |  |
|               | > Sign in                  uses Codex auth                  |  |
|               |                                                             |  |
|               |   Choose model             No model selected                |  |
|               |                                                             |  |
|               |   Choose browser           Local Chrome                     |  |
|               |                                                             |  |
|               | enter select     tab history     / actions                  |  |
|               +-------------------------------------------------------------+  |
|                                                                                |
+--------------------------------------------------------------------------------+
```

Current ready workbench:

```text
+-- browser-use                                      Local Chrome  GPT-5.5 -------+
| What should the browser do?                                                     |
|                                                                                |
| Recent                                                                         |
|                                                                                |
|   > found Hacker News top posts                                  recent         |
|   > compared pricing pages                                      recent          |
|                                                                                |
| Ready  ready      browser connected                                             |
|                                                                                |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Tell the browser what to do...                                            | |
| +----------------------------------------------------------------------------+ |
|                                      enter run     tab history     / actions    |
+--------------------------------------------------------------------------------+
```

Current running task:

```text
+-- find the top 5 Hacker News posts  running -----------------------------------+
|                                                                                |
| * browsing news.ycombinator.com                                                 |
| * connected live browser                                                        |
| * using browser                                                                 |
| * writing result                                                                |
|                                                                                |
| Browser                                                                        |
| page      https://news.ycombinator.com                                          |
| open      live browser                                                          |
|                                                                                |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Type to steer the agent...                                                | |
| +----------------------------------------------------------------------------+ |
|                                      enter steer     ctrl+c stop     f2 browser |
+--------------------------------------------------------------------------------+
```

Current result:

```text
+-- find the top 5 Hacker News posts  done --------------------------------------+
| Result                                                                         |
|                                                                                |
| Top 5 Hacker News posts                                                        |
|                                                                                |
| 1. Example story                                                               |
| 2. Another story                                                               |
| 3. Browser agents in practice                                                  |
|                                                                                |
| Source                                                                         |
| https://news.ycombinator.com                                                   |
|                                                                                |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Ask a follow-up...                                                        | |
| +----------------------------------------------------------------------------+ |
|                             enter follow-up     f2 browser     tab history     |
+--------------------------------------------------------------------------------+
```

Current failure:

```text
+-- current task  failed --------------------------------------------------------+
| The agent could not finish the task.                                            |
|                                                                                |
| OpenRouter API key is missing. Sign in before retrying.                         |
|                                                                                |
| > Retry                                                                        |
|   Sign in                                                                      |
|   Choose model                                                                 |
|   Change browser                                                               |
|                                                                                |
| Work preserved in history.                                                     |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Ask a follow-up...                                                        | |
| +----------------------------------------------------------------------------+ |
|                             enter follow-up     f2 browser     tab history     |
+--------------------------------------------------------------------------------+
```

Current browser overlay:

```text
+-- current task  done ----------------------------------------------------------+
| Result                                                                         |
|                                                                                |
|          +--Browser--------------------------------------------------------+   |
|          | Current                                                        |   |
|          | backend   Local Chrome                                         |   |
|          | title     Hacker News                                          |   |
|          | page      https://news.ycombinator.com                         |   |
|          | status    connected                                            |   |
|          | live      https://live.browser-use.com/?wss=example            |   |
|          | tabs      1 open                                               |   |
|          | viewport  1440 x 900                                           |   |
|          |                                                                |   |
|          | > Open browser                                                 |   |
|          |   Reconnect                                                    |   |
|          |   Change browser                                               |   |
|          |                                                                |   |
|          | enter select     esc close                                     |   |
|          +----------------------------------------------------------------+   |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Ask a follow-up...                                                        | |
| +----------------------------------------------------------------------------+ |
+--------------------------------------------------------------------------------+
```

Current history overlay:

```text
+-- current task  done ----------------------------------------------------------+
| Result                                                                         |
|                                                                                |
|       +--Previous work-----------------------------------------------------+   |
|       | > Find the top 5 Hacker News posts              done      recent   |   |
|       |   Compare browser automation tools              done      recent   |   |
|       |   Analyse repository structure                  failed    recent   |   |
|       |                                                                |      |
|       | enter open     r resume     esc close                         |      |
|       +---------------------------------------------------------------+      |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Ask a follow-up...                                                        | |
| +----------------------------------------------------------------------------+ |
+--------------------------------------------------------------------------------+
```

Current actions overlay:

```text
+-- current task  done ----------------------------------------------------------+
| Result                                                                         |
|                                                                                |
|                  +--Actions-----------------------------------------------+    |
|                  | > New task                                             |    |
|                  |   Open browser                                         |    |
|                  |   Previous work                                        |    |
|                  |   Setup                                                |    |
|                  |   Choose model                                         |    |
|                  |   Sign in                                              |    |
|                  |                                                        |    |
|                  | enter select     esc close                             |    |
|                  +--------------------------------------------------------+    |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Ask a follow-up...                                                        | |
| +----------------------------------------------------------------------------+ |
+--------------------------------------------------------------------------------+
```

Current key model:

```text
enter      run, follow up, steer, confirm overlay
shift+enter newline in composer
tab        previous work
f1         keyboard help
f2         browser
/          actions
esc        close overlay
ctrl+c     clear input, stop task, quit on second press
ctrl+d     hidden demo completion
ctrl+e     hidden developer overlay
arrows     overlay selection only
```

Current functionality:

```text
Task
- create a new task from composer
- follow up on a selected completed task
- steer a running task
- retry a failed task by pressing enter on empty composer
- cancel running task

Setup
- choose account
- choose model
- choose browser
- persist settings in SQLite
- show auth missing notice

Browser
- show current browser summary projected from events
- request open browser
- request reconnect browser
- change browser mode

History
- list previous work
- open previous work
- resume selected work with r

Developer
- show recent raw events for selected task
```

## WHATS WRONG

The underlying event-driven model is good. The UI model is overgrown.

Keep:

```text
append-only events
SQLite store
WorkbenchState projection
task/follow-up/retry/cancel lifecycle
browser summary events
curated model choices
deterministic dump-screen tests
Python owning browser connection/lifecycle
Rust owning TUI/state/model orchestration
```

Be brutal about the rest:

```text
The full-screen boxed dashboard fights the terminal.
The app owns scroll, cursor rendering, selection, and text editing in ways users do not expect.
The composer is custom editor code, so every terminal edge case becomes our bug.
The overlay list is too large for the actual product.
Setup, account, model, browser choice, setup complete, actions, help, history, and developer
are all competing for the same modal system.
Browser state is too important to hide behind F2.
The result is the product, but it is rendered as a small block inside a dashboard.
The result view drops context: it should show the task, what ran, then the final result.
Markdown rendering is too shallow. Strong markers like `**14 items**` leak into the UI.
History still behaves like internal session selection.
The model/account flow exposes implementation details at the wrong time.
Auth failure copy tells users to run commands instead of letting them fix it in place.
Onboarding screens are currently mostly selection screens. They must actually complete setup.
Open live browser must actually open the live browser, not just record that opening was requested.
Developer events are useful, but they leak implementation vocabulary into product code.
The UI says "browser agent cockpit" but behaves like a settings-heavy CLI dashboard.
```

Specific complexity to remove:

```text
SetupComplete overlay
  Kill it. Show "ready" inline and move on.

Help overlay
  Kill it. Footer hints should be enough.

Developer overlay
  Keep only behind --debug or a hidden command.

BrowserChoice overlay
  Fold into setup/browser panel.

Actions overlay as a destination
  Keep a tiny command palette, but do not make it a second navigation system.

Outer border around everything
  Remove it. It makes the app feel fake and causes terminal behavior surprises.

Raw session vocabulary
  Never expose it. Use task, previous work, result, browser.

Custom composer sprawl
  Isolate it hard or copy Codex behavior more directly.
```

Screen contract:

```text
Every visible screen must have real behavior behind every visible action.

If the UI says "Sign in", it must let the user sign in or store the needed key.
If the UI says "Open live browser", it must launch the live browser.
If the UI says "Reconnect", it must cause a browser reconnect and show the outcome.
If the UI says "Choose model", the chosen model must be runnable or route to the missing setup.
If the UI says "Result", it must show what was asked, what happened, and what the answer is.
If the UI renders markdown, common markdown syntax must disappear into styling, not leak as text.
```

The current state shape is this:

```text
             +----------------+
             |    Overlay     |
             +----------------+
                     ^
                     |
                     v
+---------+   +--------------+   +------------+
| Store   |-->| Workbench    |-->| Renderer   |
| events  |   | projection   |   | dashboard  |
+---------+   +--------------+   +------------+
      ^              ^                 ^
      |              |                 |
      +--------+-----+---------+-------+
               |
          +----------+
          | Composer |
          +----------+
```

That is too tangled. The renderer should not feel like everything depends on everything.

The target shape should be:

```text
+-------------+       +----------------+       +-----------------+
| Event Store | ----> | Product State  | ----> | Transcript View |
+-------------+       +----------------+       +-----------------+
       ^                       |                         |
       |                       v                         v
       |              +----------------+       +-----------------+
       +------------- | Commands       | <---- | Composer        |
                      +----------------+       +-----------------+
                               |
                               v
                      +----------------+
                      | Agent/Browser  |
                      +----------------+
```

The TUI should not be a dashboard with modals. It should be a transcript with a composer and tiny command surfaces.

## PROPOSED DESIGN (and behaviour)

The perfect terminal UI is transcript-first, Codex-like, and browser-agent-specific.

Principles:

```text
1. The result is the product.
2. Browser state is always visible enough to trust.
3. Setup appears only when needed.
4. The composer behaves like a normal terminal text input.
5. Scroll scrolls transcript/history, never the composer cursor.
6. The command palette is tiny.
7. Debug tools are hidden.
8. Internal words stay internal.
9. Every visible action must actually work.
```

Product vocabulary:

```text
Use:
task
browser
account
model
result
history
setup

Avoid in main UI:
session
artifact
trace
provider
event
tool output
agent graph
raw payload
```

Proposed top-level UI:

```text
+--------------------------------------------------------------------------------+
| browser-use                         Local Chrome connected        GPT-5.5       |
|--------------------------------------------------------------------------------|
|                                                                                |
| Transcript                                                                     |
| - user task                                                                    |
| - agent progress                                                               |
| - browser state                                                                |
| - result                                                                       |
| - follow-ups                                                                   |
|                                                                                |
|--------------------------------------------------------------------------------|
| Composer                                                                       |
| >                                                                              |
|--------------------------------------------------------------------------------|
| Footer hints                                                                   |
+--------------------------------------------------------------------------------+
```

This is the conceptual frame, but the real visual should avoid a heavy outer box. It should feel like a native terminal app:

```text
browser-use                                      Local Chrome connected   GPT-5.5
--------------------------------------------------------------------------------

You
  go to hackernews and save the top 5 posts to json

What ran
  * connected browser
  * opened news.ycombinator.com
  * read front page
  * saved hackernews_top5.json

Result
  Done, saved the top 5 Hacker News posts to:

  /Users/greg/project/hackernews_top5.json

  Source
  https://news.ycombinator.com/news

Browser
  Hacker News
  https://news.ycombinator.com/news
  open live browser

--------------------------------------------------------------------------------
> Ask a follow-up...
enter send   shift+enter newline   ctrl+c stop   / actions   tab history
```

### Proposed App States

State machine:

```text
FIRST RUN
   |
   v
SETUP NEEDED ------+
   |               |
   v               |
READY <------------+
   |
   v
RUNNING <---- user follow-up / steer
   |
   +----> RESULT ----> follow-up ----> RUNNING
   |
   +----> FAILED ----> retry/fix ----> RUNNING
   |
   +----> CANCELLED -> follow-up/new task
```

### First Run Setup

Goal: activation, not settings.

```text
browser-use setup
--------------------------------------------------------------------------------

Set up the browser agent

  [needs] Sign in        No account connected
  [needs] Model          No model selected
  [ok]    Browser        Local Chrome available

> Sign in
  Choose model
  Change browser

--------------------------------------------------------------------------------
enter continue   esc quit
```

Behavior:

```text
If no usable account exists, select Sign in by default.
If account exists but no model exists, select Choose model by default.
If both exist, select Start using browser-use.
No empty workbench before setup.
No setup-complete modal.
```

### Sign In

```text
browser-use setup / sign in
--------------------------------------------------------------------------------

Choose an account

> Codex login             connected
  Claude Code login       needs sign in
  OpenAI API key          needs key
  Anthropic API key       needs key
  OpenRouter API key      needs key

--------------------------------------------------------------------------------
enter select   esc back
```

Behavior:

```text
Selecting an API-key account should open an inline key entry state.
Do not tell users to run a separate command if we can collect the key here.
Codex login can use existing auth and should be the default.
```

Inline API key entry:

```text
browser-use setup / openrouter
--------------------------------------------------------------------------------

OpenRouter API key

  sk-or-v1-****************************************

  This key is stored locally in browser-use state.

> Save key
  Cancel

--------------------------------------------------------------------------------
enter save   esc cancel
```

### Choose Model

Do not make users pick provider first. A model row should explain which account it uses.

```text
browser-use setup / model
--------------------------------------------------------------------------------

Recommended

> GPT-5.5             Codex login          best default
  Claude Sonnet 4.6   Claude Code login    good browser agent
  Claude Opus 4.7     Claude Code login    strongest reasoning

API keys

  GPT-5.5             OpenAI API key        needs key
  Claude Sonnet 4.6   Anthropic API key     needs key
  Qwen3.6 Plus        OpenRouter API key    needs key
  GLM-5.1             OpenRouter API key    needs key
  DeepSeek V4 Pro     OpenRouter API key    needs key

Current
  none

--------------------------------------------------------------------------------
enter select   a sign in for selected   esc back
```

Behavior:

```text
Curated list only.
No model zoo.
No raw provider IDs in the main row.
If selected model requires auth, route directly into the needed sign-in flow.
After sign-in, return to the selected model.
```

### Choose Browser

```text
browser-use setup / browser
--------------------------------------------------------------------------------

Choose browser

> Browser Use cloud       remote browser with live view
  Local Chrome            visible browser on this machine
  Headless Chromium       background browser

Current
  Browser Use cloud

--------------------------------------------------------------------------------
enter select   esc back
```

Behavior:

```text
Default to remote/cloud for painless testing.
Local Chrome is optional because macOS permission dialogs are hostile.
Do not expose CDP endpoints, target IDs, or protocol details here.
```

### Ready

```text
browser-use                                      Browser Use cloud       GPT-5.5
--------------------------------------------------------------------------------

What should the browser do?

Recent
  find the top 5 Hacker News posts                            done      12m ago
  compare browser automation tools                            done      1h ago
  analyse repository structure                                failed    2h ago

Ready
  account    Codex login
  browser    connected

--------------------------------------------------------------------------------
> Tell the browser what to do...
enter run   tab history   / actions
```

Behavior:

```text
Typing starts a task.
Tab opens previous work.
/ opens command palette.
No giant onboarding text.
No hidden setup if the app is not ready. Route to setup repair instead.
```

### Running

```text
browser-use                                      Browser Use cloud       GPT-5.5
--------------------------------------------------------------------------------

You
  find the top 5 Hacker News posts and save them to json

Agent is working                                            48s

  * connected browser
  * opened news.ycombinator.com
  * reading front page
  * extracting posts

Browser
  Hacker News
  https://news.ycombinator.com/news
  live view available

--------------------------------------------------------------------------------
> Type to steer the agent...
enter steer   shift+enter newline   ctrl+c stop   f2 browser   / actions
```

Behavior:

```text
Show compact human activity, not raw tool calls.
Show current browser URL/title inline.
Follow-up text while running is steering.
Ctrl+c stops once. Second ctrl+c quits only if nothing is running.
Loading indicator should be subtle and native-feeling.
```

### Result

```text
browser-use                                      Browser Use cloud       GPT-5.5
--------------------------------------------------------------------------------

Task
  find the top 5 Hacker News posts and save them to json

What ran
  * connected browser
  * opened news.ycombinator.com/news
  * read front page
  * extracted 5 posts
  * saved hackernews_top5.json

Result

  Saved the top 5 Hacker News posts to:

  /Users/greg/project/hackernews_top5.json

  1. Example story
     382 points, 128 comments
     https://example.com/story

  2. Another story
     214 points, 76 comments
     https://example.com/another

Source
  https://news.ycombinator.com/news

--------------------------------------------------------------------------------
> Ask a follow-up...
enter follow-up   shift+enter newline   f2 browser   tab history   / actions
```

Behavior:

```text
Render markdown properly.
Always show the task, completed steps, and result together.
The completed steps should be the frozen final version of the running activity.
Strong emphasis, inline code, headings, ordered lists, unordered lists, links, and bare URLs
must render as terminal text, not raw markdown syntax.
Long list items need hanging indentation so continuation lines still belong to the item.
Links should be visually distinct and terminal-clickable where supported.
Saved paths should be visible inline.
Do not create a separate artifact browser for v1.
If the result is long, terminal scroll should review it naturally.
```

Good result rendering:

```text
Result

  Your Amazon cart currently has 14 items with a subtotal of $1,240.70:

  * Moso Natural Air Purifying Bag 600g
    qty 1, $24.95

  * AE0CKY 4500 Sq.Ft Dehumidifier, 80 Pint/Day
    qty 1, $239.97

  * MIULEE Velvet Curtains, 108", Olive Green, 2 Panels
    qty 4, $55.99 each, coupon price shown $44.79
```

Bad result rendering:

```text
Result

  Your Amazon cart currently has **14 items** with a subtotal of **$1,240.70**:

  • **MIULEE Velvet Curtains, 108", Olive Green, 2 Panels** – qty 4 –
  **$55.99 each** / coupon price shown **$44.79**
```

### Failure

```text
browser-use                                      Browser Use cloud       GPT-5.5
--------------------------------------------------------------------------------

You
  use GLM-5.1 to summarize this page

The agent could not start.

OpenRouter API key is missing.

> Sign in to OpenRouter
  Choose a different model
  Retry
  New task

Work is saved in history.

--------------------------------------------------------------------------------
enter select   esc back
```

Behavior:

```text
Failure always gives a useful next step.
The default selection should fix the actual problem.
For auth failures, route to sign-in.
For model failures, route to model selection.
For browser failures, route to browser panel.
Do not dump raw exception text as the main message.
```

### Cancelled

```text
browser-use                                      Browser Use cloud       GPT-5.5
--------------------------------------------------------------------------------

You
  compare three pricing pages

Stopped.

Progress is saved in history.

> Continue with a follow-up
  Start a new task
  Previous work

--------------------------------------------------------------------------------
enter select   / actions
```

Behavior:

```text
Cancelled is not an error.
Make continuing or starting fresh obvious.
```

### Browser Panel

This should be the only place with detailed browser status. It should still avoid protocol internals.

```text
browser-use / browser
--------------------------------------------------------------------------------

Current browser

  backend     Browser Use cloud
  status      connected
  title       Hacker News
  page        https://news.ycombinator.com/news
  live view   available
  tabs        1 open
  viewport    1440 x 900

> Open live browser
  Reconnect
  Change browser

--------------------------------------------------------------------------------
enter select   esc back
```

Behavior:

```text
Browser state is projected from browser events.
Open live browser launches the live URL in the OS browser immediately.
Reconnect should emit one clear reconnect command.
Python browser tool should own or be fully aware of reconnect state.
Never show tab IDs, target IDs, object IDs, or CDP internals in the product UI.
```

### Previous Work

```text
browser-use / previous work
--------------------------------------------------------------------------------

> find the top 5 Hacker News posts                         done        12m ago
  compare browser automation tools                         done        1h ago
  analyse repository structure                             failed      2h ago
  book a one-way flight from Ljubljana to Zurich            stopped     yesterday

--------------------------------------------------------------------------------
enter open   r resume   / actions   esc back
```

Behavior:

```text
Call it previous work or history, never sessions.
Opening shows the transcript/result.
Resuming appends a follow-up or restarts from the previous task context.
```

### Actions

Keep it tiny.

```text
Actions
--------------------------------------------------------------------------------

> New task
  Open browser
  Reconnect browser
  Previous work
  Choose model
  Sign in

--------------------------------------------------------------------------------
type to filter   enter select   esc close
```

Behavior:

```text
Actions is a command palette, not an app navigation system.
No debug items unless debug mode is enabled.
No generic "Continue"; the composer already does that.
No generic "Stop"; running state already has ctrl+c stop.
```

### Multiline Composer

```text
--------------------------------------------------------------------------------
> find flights from ljubljana to zurich
  one way
  tomorrow morning
--------------------------------------------------------------------------------
enter send   shift+enter newline   option+delete word   cmd+delete line
```

Behavior:

```text
Normal typing must feel boring and native.
Shift+enter inserts newline.
Enter sends.
Cmd+delete deletes the current line.
Repeated cmd+delete deletes one line at a time.
Option+delete deletes a word.
Ctrl+a/e move to line start/end.
Scroll never moves the composer cursor.
Plain up/down should not be composer navigation unless we implement full shell history intentionally.
Maximum visible height is about 10 lines.
Longer input scrolls inside composer without moving page scroll.
```

### Rendering Architecture

Proposed code shape:

```text
crates/browser-use-tui
|-- app.rs
|   |-- AppState
|   |-- AppCommand
|   `-- state transitions
|
|-- composer.rs
|   |-- input buffer
|   |-- cursor
|   |-- key handling
|   `-- tests copied from Codex-like behavior
|
|-- screens.rs
|   |-- ReadyScreen
|   |-- RunningScreen
|   |-- ResultScreen
|   |-- FailureScreen
|   `-- SetupScreen
|
|-- palette.rs
|   |-- action list
|   |-- filtering
|   `-- selection
|
|-- browser_panel.rs
|   |-- browser summary
|   `-- browser commands
|
|-- render.rs
|   `-- pure rendering only
|
`-- runtime.rs
    `-- agent thread integration
```

Proposed event flow:

```text
composer submit
   |
   v
AppCommand
   |
   +-- StartTask(text)
   +-- SendFollowup(text)
   +-- StopTask
   +-- RetryTask
   +-- OpenBrowser
   +-- ReconnectBrowser
   +-- ChangeModel
   +-- SaveAuth
   |
   v
Store event / setting mutation
   |
   v
WorkbenchState projection
   |
   v
Render transcript
```

### Migration Plan

Do this as a UI rewrite, not a whole-system rewrite.

```text
Phase 1: Lock product states
  - keep SQLite/events
  - define ProductState enum
  - define AppCommand enum
  - define a screen contract for every visible action
  - write ASCII snapshot tests for every state in this document

Phase 2: Extract composer
  - move composer into composer.rs
  - copy Codex behavior where it actually fits
  - explicitly reject scroll/up/down cursor bugs
  - add terminal key regression tests

Phase 3: Replace dashboard with transcript
  - remove giant outer border
  - render header + transcript + composer
  - result markdown becomes first-class
  - links/paths get proper styling

Phase 4: Collapse overlays
  - remove SetupComplete
  - remove Help overlay
  - hide Developer overlay behind debug
  - merge BrowserChoice into Browser/Setup
  - make Actions a tiny command palette

Phase 5: Fix setup
  - sign-in state collects keys where possible
  - model selection routes to required sign-in
  - no setup screen can be cosmetic only
  - remote browser/cloud is painless default

Phase 6: Polish browser trust
  - browser status always visible
  - open live browser actually launches the browser
  - browser panel is useful but not a debugger
  - reconnect semantics are explicit
  - Python browser tool owns or tracks browser lifecycle
```

Final target:

```text
The app should feel like:

  "tell the browser what to do, watch enough to trust it, steer when needed,
   then receive a useful result"

Not:

  "manage sessions, providers, artifacts, settings, traces, and terminal widgets"
```

