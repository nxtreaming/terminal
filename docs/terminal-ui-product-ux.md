# Browser Use Terminal UX

This is a product-first sketch for a much simpler terminal UI.

Assume inherited features are accidental until proven useful. The app should not preserve old screens just because they exist.

## Product Frame

The app should feel like a browser agent cockpit, not a CLI dashboard.

The user wants to:

```text
set up the agent once
tell the browser what to do
watch enough to trust it
interrupt or steer when needed
get a useful result
open the browser when needed
```

That is it.

## Product Vocabulary

Use these words:

```text
task
browser
account
model
result
history
setup
```

Avoid these words in the main UI:

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

Those may exist internally, but they are not user concepts for v1. "Provider" especially should disappear from the main UI. Users pick an account and a model. The provider is implied by that choice.

## App Model

There is one main screen: the workbench.

Everything else is temporary:

```text
Workbench
  setup flow         first-run auth, model, browser
  browser overlay    open/reconnect/change browser
  history overlay    previous work
  action menu        small contextual menu
```

The core flow is:

```text
FIRST RUN -> SETUP -> ASK

ASK -> RUNNING -> RESULT
        |
        v
      STEER / STOP

BROKEN SETUP -> SETUP -> ASK
FAILED TASK  -> RETRY / FIX
```

No artifact browser. No trace viewer. No debug panel in the default product.

## 1. First Run Setup

The first thing the user should see is setup/auth. Not the workbench.

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

This is not a generic settings page. It is the activation funnel.

## 2. Sign In

```text
+--------------------------------------------------------------------------------+
| Sign in                                                                        |
|--------------------------------------------------------------------------------|
| Choose how the agent should connect to a model.                                 |
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

The user should not have to understand providers yet. They choose the account path they actually have.

## 3. Model Selection

Model selection should combine model and provider. Provider is implementation detail, but model availability depends on the account, so the UI should show both together.

```text
+--------------------------------------------------------------------------------+
| Choose model                                                                   |
|--------------------------------------------------------------------------------|
| Recommended                                                                    |
|                                                                                |
| > GPT-5.5                         Codex login             best default         |
|   GPT-5.5                         OpenAI API key          sign in required     |
|   Claude Opus 4.7                 Claude Code login       sign in required     |
|   Claude Opus 4.7                 Anthropic API key       sign in required     |
|   Claude Sonnet 4.6               Claude Code login       sign in required     |
|   Claude Sonnet 4.6               Anthropic API key       sign in required     |
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

Rules:

```text
Do not make users pick provider first.
Do not show a model that cannot run without explaining what account it needs.
Prefer one recommended model.
Keep advanced model details hidden.
Only show SOTA models by default.
Avoid model zoo behavior.
```

The default list should be curated, not exhaustive:

```text
OpenAI
  GPT-5.5 via Codex login
  GPT-5.5 via OpenAI API key

Anthropic
  Claude Opus 4.7 via Claude Code login
  Claude Opus 4.7 via Anthropic API key
  Claude Sonnet 4.6 via Claude Code login
  Claude Sonnet 4.6 via Anthropic API key

OpenRouter
  Qwen3.6 Plus
  GLM-5.1
  DeepSeek V4 Pro
```

## 4. Browser Selection

```text
+--------------------------------------------------------------------------------+
| Choose browser                                                                 |
|--------------------------------------------------------------------------------|
| > Local Chrome                 visible browser on this machine                  |
|   Browser Use cloud            remote browser with live view                    |
|   Headless Chromium            background browser                               |
|                                                                                |
| Current                                                                        |
|   Local Chrome available                                                       |
|                                                                                |
| enter select     esc back                                                      |
+--------------------------------------------------------------------------------+
```

This should be simple. CDP endpoints, daemon mode, and internal browser modes should not be shown in the normal setup flow.

## 5. Setup Complete

```text
+--------------------------------------------------------------------------------+
| Ready                                                                          |
|--------------------------------------------------------------------------------|
| [ok] Signed in       Codex login                                                |
| [ok] Model           GPT-5.5                                                    |
| [ok] Browser         Local Chrome                                               |
|                                                                                |
| > Start using browser-use                                                       |
|                                                                                |
| enter continue                                                                 |
+--------------------------------------------------------------------------------+
```

After this, the user lands in the workbench.

## 6. Ready Workbench

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
|   > found Hacker News top posts                                  12m ago        |
|   > compared 4 pricing pages                                    yesterday       |
|                                                                                |
| Ready                                                                          |
|   signed in      browser connected                                              |
|                                                                                |
| enter run     tab history     / actions     f1 keys                             |
+--------------------------------------------------------------------------------+
```

What this screen does:

```text
start work
show recent work
show basic readiness
```

What it must not do:

```text
show a giant logo
teach the whole product
advertise settings
show internal state
```

## 7. Setup Repair Overlay

After first run, setup only reappears if the app cannot run a task or the user opens it from actions.

```text
+--------------------------------------------------------------------------------+
| Setup                                                                          |
|--------------------------------------------------------------------------------|
| The browser agent needs attention.                                              |
|                                                                                |
| [ok]  Browser       Local Chrome found                                          |
| [fix] Sign in      No usable account found                                      |
| [fix] Model         No usable model selected                                    |
|                                                                                |
| > Sign in                                                                       |
|   Choose model                                                                  |
|   Change browser                                                                |
|                                                                                |
| enter fix     esc back                                                          |
+--------------------------------------------------------------------------------+
```

Setup is a repair flow, not a destination.

## 8. Running Task

```text
+--------------------------------------------------------------------------------+
| find the top 5 Hacker News posts                            running  $0.03  48s |
|--------------------------------------------------------------------------------|
|                                                                                |
|   * browsing   news.ycombinator.com                                             |
|   * reading    front page                                                       |
|   * found      5 posts                                                          |
|   * checking   scores and comments                                              |
|                                                                                |
| Browser                                                                        |
|   page       https://news.ycombinator.com/                                      |
|   open       live browser                                                       |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Type to steer the agent...                                                | |
| +----------------------------------------------------------------------------+ |
|                                                                                |
| enter steer     ctrl+c stop     f2 browser     / actions                        |
+--------------------------------------------------------------------------------+
```

This screen answers:

```text
what is it doing?
where is the browser?
can I steer or stop it?
```

It should not show every tool call. It should summarize behavior in human language.

## 9. Result

```text
+--------------------------------------------------------------------------------+
| find the top 5 Hacker News posts                               done  $0.06  2m03s|
|--------------------------------------------------------------------------------|
| Result                                                                         |
|                                                                                |
| Top 5 Hacker News posts                                                        |
|                                                                                |
| 1. Bun's experimental Rust rewrite hits 99.8% test compatibility               |
|    299 points, 300 comments                                                     |
|    https://twitter.com/...                                                      |
|                                                                                |
| 2. Internet Archive Switzerland                                                 |
|    493 points, 72 comments                                                      |
|    https://blog.archive.org/...                                                 |
|                                                                                |
| Source                                                                         |
|   news.ycombinator.com                                                          |
|                                                                                |
| +----------------------------------------------------------------------------+ |
| | > Ask a follow-up...                                                        | |
| +----------------------------------------------------------------------------+ |
|                                                                                |
| enter follow-up     f2 browser     tab history     / actions                    |
+--------------------------------------------------------------------------------+
```

The result is the product. The log is secondary.

If the user asks to save something, the result can show the saved path inline:

```text
Saved
  /Users/greg/Desktop/hn_posts.json
```

We do not need an artifact system to expose that.

## 10. Browser Overlay

This exists for two reasons: prove the browser is real and let the user open or fix it.

It can show useful CDP-derived state, but it should not become a CDP debugger.

```text
+--------------------------------------------------------------------------------+
| Browser                                                                        |
|--------------------------------------------------------------------------------|
| Current                                                                        |
|   backend      local chrome                                                     |
|   title        Hacker News                                                     |
|   page         https://news.ycombinator.com/                                    |
|   status       connected                                                        |
|   tabs         1 open                                                           |
|   viewport     1440 x 900                                                       |
|                                                                                |
| > Open browser                                                                  |
|   Reconnect                                                                     |
|   Change browser                                                                |
|                                                                                |
| enter select     esc close                                                      |
+--------------------------------------------------------------------------------+
```

Good browser state:

```text
backend
connection status
current page title
current URL
live browser link
tab count
viewport size
last navigation error, only if relevant
```

Bad browser state:

```text
raw CDP events
frame tree dumps
network waterfalls
console logs by default
protocol IDs
target IDs
```

This replaces separate browser, evidence, artifact, and trace screens for the main product.

## 11. Failure

```text
+--------------------------------------------------------------------------------+
| analyse the current repository                                   failed  $0.02  |
|--------------------------------------------------------------------------------|
| The agent could not reach the model.                                            |
|                                                                                |
| Read timed out while connecting to chatgpt.com.                                 |
|                                                                                |
| > Retry                                                                         |
|   Sign in                                                                       |
|   Choose model                                                                  |
|   Change browser                                                                |
|   Stop                                                                          |
|                                                                                |
| Work preserved in history.                                                      |
|                                                                                |
| enter select     esc back                                                       |
+--------------------------------------------------------------------------------+
```

A failure should always offer a next step.

## 12. History Overlay

Call it history or previous work, not sessions.

```text
+--------------------------------------------------------------------------------+
| Previous work                                                                  |
|--------------------------------------------------------------------------------|
| > find the top 5 Hacker News posts                          done      12m ago   |
|   compare browser automation tools                          done      1h ago    |
|   analyse repository structure                              failed    2h ago    |
|                                                                                |
| enter open     r resume     esc close                                           |
+--------------------------------------------------------------------------------+
```

The user is resuming work, not managing sessions.

## 13. Actions Menu

This should stay tiny.

```text
+--------------------------------------------------------------------------------+
| Actions                                                                        |
|--------------------------------------------------------------------------------|
| > New task                                                                      |
|   Open browser                                                                  |
|   Previous work                                                                 |
|   Setup                                                                         |
|   Choose model                                                                  |
|                                                                                |
| type to search     enter select     esc close                                   |
+--------------------------------------------------------------------------------+
```

No generic `Continue`. The composer already handles follow-ups.

No generic `Stop` in the default menu. Stop is a running-state control, shown in the footer as `ctrl+c stop`.

No advanced section for v1.

If we later prove the need for debug tools, they can be added behind a hidden developer mode.

## Keyboard

Keep this tiny:

```text
enter      run, follow up, confirm
tab        previous work
f2         browser
/          actions
ctrl+c     clear input, stop task, or quit with confirmation
esc        close overlay
```

If the app needs more shortcuts than this, the UI model is too complicated.

## Design Principle

Hide complexity until the user asks for it.

The app should say:

```text
set me up
give me the task
I will show my work
you can take over any time
```
