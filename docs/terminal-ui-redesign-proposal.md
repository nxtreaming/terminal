# Terminal UI Redesign Proposal

## Goal

Make the Rust terminal UI feel like a modern coding assistant while keeping the
current product simple:

```text
plain transcript
strong prompt box
compact status
small command surfaces
native terminal scrollback
browser-first language
```

This is not a proposal to add more screens. It is a proposal to make the existing
interaction model feel intentional.

## Current Problem

The current app works, but it looks and behaves like a debug log with a prompt
attached.

The biggest visible failure is the real terminal layout:

```text
large blank terminal area








browser-use
--------------------------------------------------------------------------------
> prompt
  +- result
  |  ...
> Ask a follow-up...
```

Root cause:

```text
The live TUI only owns a tiny inline viewport.
Completed transcript content is inserted into terminal scrollback.
On tall terminals, the live app gets pinned near the bottom of a mostly empty
terminal surface.
```

That architecture was useful for proving native scrollback, but it is not good
enough for the product UI.

## What To Keep

Keep these decisions:

```text
append-only event store
WorkbenchState projection
native selectable transcript output
plain terminal text for completed work
simple keyboard model
history, browser, actions, setup, model surfaces
follow-up flow on completed tasks
ctrl+c clears input, stops running task, then quits with confirmation
```

The current app has the right product bones. The redesign should not turn it
into a dashboard.

## What To Change

Change these:

```text
live viewport should own the visible terminal
composer should become the primary visual object
native transcript should not render live controls
remove "+-" block styling
remove duplicated next/action menus
move status into the composer/status strip
use a wide right rail only when space allows
```

## Non-Goals

Do not add these in this pass:

```text
mouse support
artifact browser
full model marketplace
trace viewer
CDP/network debugger
rich devtools panel
per-tool event firehose
decorative logo screen
startup marketing screen
```

## Architecture Direction

Use two renderers.

```text
1. Live UI renderer
   Owns the visible terminal area.
   Contains current task state, composer, status strip, overlays, and actions.

2. Native transcript renderer
   Emits plain selectable scrollback.
   Contains prompts, activity summaries, results, errors, and sources.
   Contains no footer, composer, headers, or selectable menus.
```

This split is the main design decision.

Current mixed model:

```text
native scrollback renderer sometimes reuses full UI state renderers
full UI state renderers include recovery menus and footer/composer context
therefore scrollback and live controls get mixed together
```

Desired model:

```text
native transcript = history of what happened
live viewport = what the user can do now
```

## Terminal Layout

The live UI should fill the visible terminal height.

```text
+------------------------------------------------------------------------------+
| current transcript preview / current state                                    |
|                                                                              |
| optional wide right rail                                                       |
|                                                                              |
|                                                                              |
| composer                                                                      |
| status and key hints                                                           |
+------------------------------------------------------------------------------+
```

Native transcript can still be inserted above the live UI, but the live UI should
not be a 10-row island at the bottom of the terminal.

## Visual Language

Use plain labels, indentation, and a strong composer.

Avoid this:

```text
  +- browser
  |  opened news.ycombinator.com
  |  connected live browser
  +- done
```

Use this:

```text
browser
  opened news.ycombinator.com
  live view connected
```

Avoid this:

```text
browser-use                                                                    Browser Use cloud connected   GPT-5.5
--------------------------------------------------------------------------------
```

Use status in the composer:

```text
+------------------------------------------------------------------------------+
| Ask a follow-up...                                                            |
|                                                                              |
| Done  GPT-5.5  Codex login        Browser Use cloud connected                 |
+------------------------------------------------------------------------------+
```

## Color Roles

Keep color sparse.

```text
blue     active prompt, selected row, browser/open link affordance
green    connected, done
amber    running, warning
red      failed, destructive/error
muted    metadata, status values, keyboard hints
white    prompt and result text
```

Most headings should be muted or plain. Only the active row and live state need
color.

## Empty-State Wordmark

Use a one-line block wordmark on the ready workbench, even when previous work is
shown underneath. This borrows the recognizable coding-assistant launch feel
without turning selected task views into branded splash screens.

```text
▄
█▀▀▄ █▀▀█ █▀▀█ █   █ █▀▀ █▀▀ █▀▀█   █  █ █▀▀ █▀▀
█▀▀▄ █▄▄▀ █  █ █▄█▄█ ▀▀█ █▀▀ █▄▄▀   █  █ ▀▀█ █▀▀
▀▀▀  ▀ ▀▀ ▀▀▀▀  ▀ ▀  ▀▀▀ ▀▀▀ ▀ ▀▀   ▀▀▀▀ ▀▀▀ ▀▀▀
```

Rules:

```text
show on the ready workbench, including when previous work exists
hide as soon as there is a selected task, setup repair, or an overlay
keep it one line conceptually: "Browser Use", not separate brand panels
do not show it in scrollback
do not show it above completed results
```

## Main Screens

### Empty Ready

```text
               █▀▀▄ █▀▀█ █▀▀█ █   █ █▀▀ █▀▀ █▀▀█   █  █ █▀▀ █▀▀
               █▀▀▄ █▄▄▀ █  █ █▄█▄█ ▀▀█ █▀▀ █▄▄▀   █  █ ▀▀█ █▀▀
               ▀▀▀  ▀ ▀▀ ▀▀▀▀  ▀ ▀  ▀▀▀ ▀▀▀ ▀ ▀▀   ▀▀▀▀ ▀▀▀ ▀▀▀



+------------------------------------------------------------------------------+
| Ask the browser to do anything...                                             |
|                                                                              |
| Build  GPT-5.5  Codex login        Browser Use cloud ready                    |
+------------------------------------------------------------------------------+
                                      tab history   / actions   f2 browser
```

Notes:

```text
No giant logo.
No setup copy unless setup is actually incomplete.
No recent section if there is no history.
The prompt box is the anchor.
```

### Ready With History

```text
browser-use

previous work
  Find the top 5 Hacker News posts                         done      3m ago
  Compare Azure billing requirements                       done      1h ago
  Inspect checkout flow                                    stopped   2h ago


+------------------------------------------------------------------------------+
| Tell the browser what to do...                                                |
|                                                                              |
| Build  GPT-5.5  Codex login        Browser Use cloud ready                    |
+------------------------------------------------------------------------------+
                                      tab history   / actions   f2 browser
```

### Running

```text
> Find the top 5 Hacker News posts

working
  running browser task

browser
  news.ycombinator.com
  live view available


+------------------------------------------------------------------------------+
| Type to steer the agent...                                                    |
|                                                                              |
| Working  GPT-5.5  Codex login      Browser Use cloud connected                |
+------------------------------------------------------------------------------+
                         enter send   shift+enter newline   ctrl+c stop
```

### Result

```text
> Find the top 5 Hacker News posts

browser
  opened news.ycombinator.com
  live view connected

result
  Top 5 Hacker News posts

  1. Example story
  2. Another story
  3. Browser agents in practice

source
  https://news.ycombinator.com


+------------------------------------------------------------------------------+
| Ask a follow-up...                                                            |
|                                                                              |
| Done  GPT-5.5  Codex login        Browser Use cloud connected                 |
+------------------------------------------------------------------------------+
                                      tab history   / actions   f2 browser
```

### Failed

```text
> Read-only exploration: summarize this repository.

error
  The agent could not start.
  spawn command via shell bash in /root/repo-explorer

next
  > Retry
    Choose a different model
    New task


+------------------------------------------------------------------------------+
| Ask a follow-up...                                                            |
|                                                                              |
| Failed  GPT-5.5  Codex login      Browser Use cloud ready                     |
+------------------------------------------------------------------------------+
                                                        enter choose   / actions
```

Auth-specific failed state:

```text
error
  OpenRouter API key is missing.

next
  > Sign in to OpenRouter
    Choose a different model
    Retry
    New task
```

Browser-specific failed state:

```text
error
  Could not connect to Local Chrome.

next
  > Open browser settings
    Choose a different browser
    Retry
    New task
```

### Stopped

```text
> Find the top 5 Hacker News posts

stopped
  Progress is saved in history.

next
  > Continue with a follow-up
    Start a new task
    Previous work


+------------------------------------------------------------------------------+
| Ask a follow-up...                                                            |
|                                                                              |
| Stopped  GPT-5.5  Codex login     Browser Use cloud connected                 |
+------------------------------------------------------------------------------+
                                                        enter choose   / actions
```

## Wide Layout

For wide terminals, add a right rail. Do not add it on narrow terminals.

Suggested threshold:

```text
width >= 120
```

Running wide layout:

```text
> Find the top 5 Hacker News posts                         | Browser
                                                           | connected
working                                                    | Hacker News
  running browser task                                     | news.ycombinator.com
                                                           | live view available
browser                                                    |
  opened news.ycombinator.com                              | Task
  live view connected                                      | running
                                                           | 1 tab
                                                           | 1440 x 900


+----------------------------------------------------------+
| Type to steer the agent...                               |
|                                                          |
| Working  GPT-5.5  Codex login                            |
+----------------------------------------------------------+
                         enter send   ctrl+c stop          tab history   / actions
```

Rail rules:

```text
show browser status, title, page, live view, tabs, viewport
show task status and elapsed/updated time if available
do not show raw event payloads
do not show trace/debug details
hide the rail before wrapping important transcript content badly
```

## Overlays

Overlays should feel like command surfaces, not separate applications.

### Actions

```text
actions

  > New task
    Open browser
    Reconnect browser
    Previous work
    Choose model
    Use Claude Code subscription
    OpenRouter models
    Sign in to Claude Code
    Sign in to OpenRouter
    Sign in
    Configure Laminar

                                      type filter   enter choose   esc close
```

Filtered:

```text
actions

filter  router

  > OpenRouter models
    Sign in to OpenRouter

                                      type filter   enter choose   esc close
```

### History

```text
previous work

  > Find the top 5 Hacker News posts                         done      3m ago
    Compare Azure billing requirements                       done      1h ago
    Inspect checkout flow                                    stopped   2h ago

                                          enter open   r resume   esc back
```

### Browser

```text
Browser Use cloud

status    connected
page      https://news.ycombinator.com
title     Hacker News
live      available
tabs      1
viewport  1440 x 900

  > Open live browser
    Reconnect
    Change browser

                                                        enter choose   esc back
```

### Setup

```text
browser-use setup

account     Codex login connected
model       not selected
browser     Browser Use cloud

  > Choose model
    Sign in
    Change browser

                                                        enter choose   esc back
```

Setup is only for first-run activation and repair. It should not become a normal
destination.

### Model

```text
choose model

recommended
  > GPT-5.5                         Codex login             best default
    Claude Opus 4.7                 Claude Code login       strongest reasoning
    Claude Sonnet 4.6               Claude Code login       good browser agent

api keys
    GPT-5.5                         OpenAI API key          needs key
    Claude Sonnet 4.6               Anthropic API key       needs key
    Claude Opus 4.7                 Anthropic API key       needs key

openrouter
    Qwen3.6 Plus                    OpenRouter API key      needs key
    Kimi K2.5                       OpenRouter API key      vision + tools
    DeepSeek V4 Pro                 OpenRouter API key      needs key

current
  GPT-5.5 via Codex login

                                                        enter choose   esc back
```

## Keyboard Model

Keep it small:

```text
enter          submit or choose
shift+enter    newline
tab            previous work
f2             browser
/              actions
esc            close surface
ctrl+c         clear input, stop task, then quit with confirmation
ctrl+e         developer, if we keep it
```

Do not add more global shortcuts until the main layout is stable.

## Implementation Order

### PR 1: Fix Space Ownership

```text
1. Make live terminal UI own the visible terminal height.
2. Keep native scrollback, but stop treating the live viewport as 8-10 rows.
3. Add smoke tests for large blank gaps.
4. Add smoke tests for selecting history from a scrolled/tall terminal.
```

Acceptance:

```text
No screenshot should show a huge empty terminal with the app pinned to the bottom.
Composer remains visible.
Native output remains selectable.
No raw escape sequences appear.
```

### PR 2: Split Native Transcript And Live Controls

```text
1. Add transcript-only render functions.
2. Remove headers, footers, composer, and next menus from native transcript replay.
3. Keep next actions in the live renderer only.
4. Verify failed/stopped selected sessions show one next menu.
```

Acceptance:

```text
Native scrollback reads like a plain task transcript.
Live viewport reads like the current interactive app.
No duplicated "next" blocks.
```

### PR 3: Composer Surface

```text
1. Replace prompt line plus dashed footer with a composer box.
2. Move model/account/browser state into the composer status strip.
3. Preserve multiline editing and paste behavior.
4. Preserve cursor correctness in real tmux tests.
```

Acceptance:

```text
The input area is visually obvious.
Users can identify model/account/browser without scanning the header.
Existing composer tests still pass.
```

### PR 4: Transcript Visual Cleanup

```text
1. Replace "+-" block grammar with simple labels and indentation.
2. Rebalance result/source/error/next visual hierarchy.
3. Keep markdown result rendering clean.
4. Update deterministic dumps and smoke expectations.
```

Acceptance:

```text
The transcript looks like assistant output, not debug output.
Result content is the visual priority.
Activity is readable but secondary.
```

### PR 5: Optional Wide Rail

```text
1. Add right rail at width >= 120.
2. Show browser/task metadata only.
3. Hide the rail on narrow terminals.
4. Verify wrapping and mobile/narrow terminal behavior.
```

Acceptance:

```text
Wide terminals use space well.
Narrow terminals stay simple.
The rail never steals room from important result text.
```

## Test Plan

Extend `scripts/tui-terminal-smoke.py`.

Add assertions:

```text
no more than 8 consecutive blank visible lines in normal live views
composer visible near bottom after starting a new session
composer visible near bottom after selecting history
composer visible near bottom after failed retry
failed selected task has exactly one next section
stopped selected task has exactly one next section
native replay contains no footer text
native replay contains no composer placeholder
native replay contains no action menu unless it was part of the result text
```

Keep existing checks:

```text
no duplicate app chrome
no leaked escape sequences
bracketed paste markers do not leak
arrow keys are consumed inside surfaces
completed non-interactive output is plain text
deterministic dumps still cover setup, ready, running, result, browser, history, actions, developer
```

## Decisions Needed

Before implementing, decide:

```text
1. Use full-height inline viewport, or alternate screen plus explicit transcript export?
2. Keep "Build" as the mode label, or rename it to "Browse", "Task", or "Agent"?
3. Should right rail ship in the first visual pass or wait until the composer lands?
4. Should Developer remain ctrl+e or be hidden behind actions?
5. Should setup use the composer box style or stay as a plain list?
```

Recommended answers:

```text
1. Full-height inline viewport first.
2. Keep Build for now, revisit after visual pass.
3. Wait on right rail until composer and transcript split are done.
4. Keep ctrl+e for now because it is already hidden enough.
5. Keep setup as a plain list.
```

## Final Target

The redesigned app should feel like:

```text
OpenCode-style prompt confidence
Codex-style compact status
Claude-style readable terminal rhythm
Browser Use-specific browser state and live view control
```

It should not copy those products directly. It should borrow the convention users
already understand: the prompt box is the center, transcript is plain, controls
are compact, and status is always visible but quiet.
