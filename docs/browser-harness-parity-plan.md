# Browser-Harness Parity Plan

This is the browser-specific parity goal for the Rust implementation in
`~/new-core/terminal-aligned-browser-harness`.

The existing Codex agent-alignment loop intentionally excludes browser
interaction parity. This document defines that separate loop: make this Rust
browser terminal/harness behave as close as possible to Codex plus
browser-harness for real browser tasks.

## What You Asked For

You described the current gap this way:

> so for example Codex (/Users/greg/Downloads/tmp/codex) agent + browser harness (/Users/greg/Developer/browser-harness) works much better than our current llm-browser (this repo).
>
> Would this PR bring us closer to the performance of codex + bh? What are the main differences

Then you clarified the target:

> ok so let's say the agent is now much more aligned.
>
> How can we bring the gap to 0 even for the browser harness ?I really want behaviiour to be as close as possible!!

You called out three areas that must be explicit:

> Prompt/tool alignment
> Profile/domain workflows
> HTTP/proxy behavior
>
> you don't really talk about these?
>
> could these be part of the plan?

Finally, you asked for this planning document:

> can you create a plan for HOW to do that (in the beggining just explain what I told you to do (like my exact user messages)) then create a high level goal for doing this (just turn this text into a plan and put it to md)

You later corrected the local implementation location:

> i mean ~/new-core/terminal-aligned-browser-harness -> this is the lation of where we ar eimplementing stuf haha
>
> amazing, can you pls create a good goal with a bit more planning

The old `/Users/greg/...` paths above are historical context from the original
request. The working reference locations for this VM are:

- Codex reference: `~/repos/codex`
- Browser-harness reference: `~/repos/browser-harness`
- Implementation target: `~/new-core/terminal-aligned-browser-harness`

## Goal

Close the browser-task behavior gap between this Rust browser terminal/harness
and `Codex + browser-harness` for supported workflows.

The target is observable parity, not identical internals. Given the same browser
task, the agent should receive the same kind of prompt guidance, see the same
browser capabilities, make similar tool choices, use the same profile/domain
knowledge, fall back to HTTP/proxy extraction in the same situations, recover
from the same browser states, and produce comparable final artifacts.

"Gap to zero" means that remaining differences are either:

- explicit product decisions,
- external limitations such as auth, website nondeterminism, or unavailable
  cloud infrastructure,
- or documented reference gaps with tests showing the exact unsupported edge.

Unknown behavioral drift is not acceptable.

## Reference Contract

Browser-harness is the reference for browser interaction behavior. Codex is the
reference for agent prompt assembly, tool presentation, runtime persistence, and
model-visible recovery heuristics.

The Rust implementation can keep different internal architecture, but these
agent-facing surfaces should converge:

- prompt text and ordering
- browser tool names, schemas, descriptions, and examples
- helper semantics and return shapes
- profile and domain-selection workflows
- domain skill discovery and loading
- HTTP/proxy fallback behavior
- screenshot, artifact, and download behavior
- browser state recovery and stale-target handling
- conformance evidence for real tasks

## Browser-Script Helper Compatibility Contract

For helpers exposed through `browser_script`, browser-harness is the source of
truth when a helper name overlaps. Rod is the CDP-level sanity check: if a helper
claims to simulate a physical browser action, its CDP events should look like a
normal Rod or browser-harness path, not a custom mixture the model has to
remember.

The immediate input contract is:

- `press_key(key, modifiers=0)` simulates a physical key or shortcut. Printable
  unmodified keys send one text-bearing `Input.dispatchKeyEvent` keydown and a
  matching keyup. Modified shortcuts such as `Meta+A` send key events without
  text insertion. It must not emit a second manual `char` event for the same
  character.
- `type_text(text)` is text insertion or paste semantics. It maps directly to
  `Input.insertText` for the focused element.
- `fill_input(selector, text)` is the default for React, Vue, Svelte, Polaris,
  and other controlled inputs. It focuses the element, clears with raw
  Cmd/Ctrl+A plus Backspace, types via physical key events, and dispatches final
  `input` and `change` events. Direct `element.value = ...` mutation is not the
  default path because it can visually fill a composer while leaving framework
  state and submit buttons disabled.

Initial helper audit matrix:

| Helper area | Compatibility target | Status |
| --- | --- | --- |
| `press_key` | Must match browser-harness physical key semantics; Rod-style separation between key events and text insertion | Covered by unit tests for chords, no manual `char`, and exact printable key events |
| `type_text` | Must map to `Input.insertText` | Covered by unit test |
| `fill_input` | Must use browser-harness focus, clear, physical typing, and final event path | Covered by unit tests plus an ignored real-browser controlled-textarea smoke |
| `click_at_xy`, mouse, scroll | Should match browser-harness unless runtime constraints require a documented adaptation | Pending audit |
| screenshots, viewport, image emission | Should match browser-harness model-visible artifact and image behavior | Pending audit |
| tabs, navigation, lifecycle waits | Should match browser-harness helper names, return shapes, and stale-target recovery behavior | Pending audit |
| uploads, downloads, dialogs, network, `http_get` | Should match browser-harness behavior where exposed to the agent | Pending audit |
| terminal-only helpers | May diverge only with explicit docs and prompt wording | Pending audit |

The work order for helper parity is input and keyboard first, mouse/click/scroll
next, screenshots and viewport after that, then tabs/navigation, then
uploads/downloads/dialogs/network.

## Workstreams

### 1. Prompt And Tool Contract Parity

Make the agent see the browser surface the way Codex sees browser-harness.

Deliverables:

- snapshot Codex plus browser-harness browser prompt text, tool schemas, helper
  names, and model-visible examples
- diff those snapshots against this repo's `browser` and `browser_script`
  prompt/tool surface
- align screenshot-first guidance, coordinate-click bias, first navigation
  semantics, domain-skill instructions, helper-writing guidance, and
  anti-framework language
- add golden tests for prompt and tool-schema drift

Acceptance evidence:

- a checked-in snapshot or fixture showing the reference prompt/tool contract
- a local generated snapshot from this repo
- a diff that is either empty or annotated with intentional differences
- tests that fail when helper names, schemas, or critical guidance drift

### 2. Profile And Domain Workflow Parity

Port browser-harness behavior around real Chrome profiles, cloud profiles,
remote daemons, remembered domain choices, and login-sensitive task routing.

Deliverables:

- audit browser-harness profile commands and this repo's `browser profile`,
  `browser connect`, `browser local`, `browser remote`, and managed-browser
  behavior
- align profile discovery output, exact-id/name collision handling, remembered
  domain profile settings, and next-step guidance
- make domain/profile choice visible before first navigation when login is
  likely to matter
- make `goto_url()` or the closest equivalent surface matching domain skill
  files and profile hints at the right time
- add task-level flows for "ask the user which profile to use", "remember this
  domain", and "reuse remembered profile"

Acceptance evidence:

- profile workflow fixtures for no profiles, one matching profile, duplicate
  names, remembered domain, forgotten domain, and unavailable cloud profile
- a real-terminal or CLI smoke flow showing the agent gets the expected next
  step before attempting login work
- model-visible output that names the selected or remembered profile without
  exposing secrets

### 3. HTTP And Proxy Behavior Parity

Match browser-harness `http_get` behavior closely enough that agents can switch
from browser exploration to direct extraction using the same strategy.

Deliverables:

- audit browser-harness `http_get`, proxy configuration, timeout behavior,
  gzip/encoding handling, header defaults, binary/text return behavior, and
  anti-bot fallback guidance
- implement the same local urllib/direct fetch fallback shape where applicable
- use documented exe.dev proxy features only when proxy behavior is needed
- support Browser Use API proxy or equivalent configured proxy behavior where
  available
- align error text so the model knows when to retry in browser, use JS fetch,
  use a proxy, or stop because anti-bot/login blocked extraction

Acceptance evidence:

- tests for text, JSON, binary, gzip, timeout, non-2xx, redirect, and custom
  header cases
- one conformance script that discovers a stable endpoint in-browser and then
  extracts through `http_get`
- documented behavior for unavailable proxy configuration

### 4. Helper Runtime Parity

Align the observable semantics of helpers that browser tasks rely on.

Deliverables:

- compare and close gaps for `new_tab`, `goto_url`, `scroll`, `wait_for_load`,
  `wait_for_network_idle`, `capture_screenshot`, `screenshot`,
  `screenshot_clip`, `click_at_xy`, `fill_input`, `type_text`, `press_key`,
  `page_info`, `js`, `current_tab`, `list_tabs`, `switch_tab`,
  `ensure_real_tab`, `upload_file`, `drain_events`, `http_get`,
  `copy_artifact`, `artifact_root`, `outputs_dir`, `session_metadata`,
  `audit_artifact`, `agent_workspace`, and `load_agent_helpers`
- align iframe targeting, shadow DOM escape hatches, dialogs, downloads,
  uploads, stale target recovery, and page lifecycle waits
- keep helper implementation thin and CDP-friendly
- prefer behavior-compatible adaptations over copying Python internals when Rust
  owns the browser state

Acceptance evidence:

- helper-level conformance tests with equivalent browser-harness scripts
- visual artifact checks for screenshot helpers
- real browser smoke tests for tabs, scroll, click, input, upload/download,
  dialog, iframe, and stale-target recovery

### 5. Domain Skill Corpus Parity

Use browser-harness accumulated site knowledge instead of forcing the model to
rediscover known selectors, private APIs, and workflow quirks.

Deliverables:

- decide whether to vendor, mount, or sync
  `~/repos/browser-harness/agent-workspace/domain-skills`
- define trust, update, and review rules for imported domain skills
- make navigation to a matching domain surface the relevant skill files
- instruct the agent to read matching domain skills before inventing selectors
  or flows
- record when a task used, ignored, or updated a domain skill

Acceptance evidence:

- tests for domain matching, subdomain matching, no-match behavior, and multiple
  matching skills
- a task trace showing the agent receives and uses a matching domain skill
- a documented update path for adding newly discovered site knowledge

### 6. Browser Runtime Recovery Parity

Make recovery from common browser states feel like browser-harness.

Deliverables:

- audit local and reference behavior for disconnected CDP, closed tabs, target
  crashes, browser restarts, stale active targets, navigation timeout, auth
  walls, downloads, and modal dialogs
- align `browser status --json`, `browser doctor`, connect/reconnect commands,
  and model-visible recovery text
- ensure recovery is explicit enough for this product while preserving the
  browser-harness task strategy

Acceptance evidence:

- smoke flows for closed active tab, disconnected browser, crashed target,
  blocked navigation, and manual relaunch
- terminal output without duplicate app chrome, leaked escape sequences, stale
  redraws, or broken paste behavior when TUI surfaces are touched

### 7. Conformance And Eval Loop

Build a repeatable loop that compares both systems on the same browser tasks.

Deliverables:

- a task corpus covering login-sensitive sites, public browsing, extraction,
  downloads, uploads, iframe/shadow DOM, infinite scroll, SPA navigation,
  anti-bot-ish failures, and final artifact creation
- a runner that captures prompts, tool schemas, helper calls, screenshots,
  artifacts, HTTP logs, browser logs, final answers, and errors
- a delta classifier with these buckets:
  - prompt/tool contract
  - profile/domain workflow
  - HTTP/proxy behavior
  - helper runtime
  - domain skill corpus
  - browser runtime recovery
  - agent runtime, model, or provider behavior
- a standing rule that the largest recurring bucket gets closed first

Acceptance evidence:

- checked-in conformance report template
- `scripts/browser-parity-snapshot.py` captures prompt/helper/domain-skill
  availability against `~/repos/browser-harness`
- at least one reference run from Codex plus browser-harness
- at least one local run from this repo
- bucketed deltas with owner, status, evidence, and next action

## Implementation Order

1. Establish the reference snapshots.

   Capture Codex plus browser-harness prompt/tool/helper/profile/http behavior
   before changing local code. This gives every later change a fixed comparison
   target.

2. Close prompt and helper contract gaps.

   This is the fastest path to better model behavior because it changes what the
   agent thinks is possible and how it chooses tools.

3. Close profile and domain workflow gaps.

   Login-sensitive browser tasks fail early when the wrong profile is selected
   or when the model does not know it should ask. This should be fixed before
   deep site-specific work.

4. Close HTTP/proxy extraction gaps.

   Many browser-harness wins come from switching out of the browser after
   discovering stable URLs, API calls, embedded JSON, pagination, or downloads.

5. Integrate the domain skill corpus.

   Once the profile and navigation surfaces are correct, make accumulated site
   knowledge available at the moment it changes the agent's choices.

6. Harden runtime recovery.

   Make common broken browser states recoverable with the same model-visible
   hints and comparable helper behavior.

7. Run conformance continuously.

   Every browser parity change should add or update a comparison case so the
   gap does not reopen silently.

## Definition Of Done

This browser parity goal is done when:

- the same supported browser task can run through Codex plus browser-harness and
  this Rust implementation with comparable practical results
- prompts, tool schemas, helper names, and critical browser guidance are covered
  by snapshots or tests
- profile/domain choices are remembered, surfaced, and used before login-sensitive
  navigation
- `http_get` and proxy fallbacks behave close enough for the same extraction
  strategy to work
- matching domain skills are discoverable and model-visible before the agent
  invents site flows
- helper runtime behavior is covered by conformance scripts and real browser
  smoke tests
- remaining differences are listed as explicit product choices, external
  blockers, or unsupported edges with evidence

For any implementation that touches `crates/browser-use-tui`, terminal output,
keyboard handling, overlays, terminal state, or Ratatui rendering, completion
also requires `scripts/verify-terminal-ui.sh` and inspection of
`/tmp/but-design-loop/`, per `AGENTS.md`.
