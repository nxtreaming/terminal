# Agent Alignment Experiment

This document explains the experiment in human terms. The working gap details
live in `docs/agent-gap-log.md`.

## Objective

Make this repository's Rust terminal/browser agent behave like Codex for
agent-quality behavior, while keeping this project's own terminal UI and browser
integration. The current objective supersedes the first feature-by-feature loop:
it is now subsystem-census-driven, so every Codex behavior surface that can
affect agent quality must be inventoried before it can be called closed.

The target is not vague inspiration from Codex. The target is behavioral parity:
the same practical heuristics for planning, editing, tool use, shell execution,
recovery from failures, verification, communication, delegation, state/history
reconstruction, model/config metadata, provider retry/streaming behavior, and
turn finalization.

Browser interaction parity is intentionally out of scope for this loop. That
is handled by the separate browser-specific goal in
`docs/browser-harness-parity-plan.md`.

## Reference Systems

- Codex repo: `/home/exedev/repos/codex`
- Local repo: `/home/exedev/new-core/terminal`

## What We Will Compare

- Agent instructions, prompt assembly, and priority handling
- Collaboration modes and Plan-mode behavior
- Planning and persistence heuristics
- Terminal command execution behavior
- Filesystem editing safety and git hygiene
- Tool schemas and tool-call decision logic
- Runtime state, history, and resume behavior
- Error handling and retry behavior
- Provider request/streaming behavior and token/usage lifecycle
- Model catalog, config layering, and personality/base-instruction metadata
- Delegation, fork/resume, mailbox, and subagent lifecycle behavior
- Turn completion, abort, finalization, and interruption behavior
- TUI rendering, keyboard flows, overlays, and terminal output
- Verification expectations before calling work complete

## How Each Iteration Works

Each iteration starts by reviewing the subsystem coverage matrix, then auditing
the currently open rows. Subagents should be used when useful to inspect
multiple Codex subsystems in parallel. The implementation target for a turn is
not one small issue; it is the largest coherent cluster of related heuristic
gaps that can be fixed and verified together without making the evidence muddy.

Before any area is marked closed, the relevant Codex source surface and local
equivalent must be inspected, model-visible/state/runtime effects must be
understood, and the result must be documented. If a gap should not be closed,
the reason is documented with evidence that it is product wiring rather than an
agent-quality behavior.

When a feature is implemented, it should also be tested against Codex itself
using a subagent with Codex auth whenever the behavior can be exercised through
Codex. That reference check is part of the acceptance gate, not optional
research.

Each loop that can affect provider transport, request shaping, tool runtime, or
subagent behavior also runs a cheap Codex-auth health smoke: a direct root
`run-codex` prompt and a spawned child `run-codex-session` reading a tiny local
file. If the smoke fails, fixing that blocker takes priority over more parity
work.

The working rule is simple: if a difference can affect agent quality, it stays
open until it is either fixed or deliberately rejected with evidence. If several
related differences can be fixed in the same loop, fix all of them in that loop.

## Current State

- Branch: `agent-gap-zero`
- Gap log: `docs/agent-gap-log.md`
- Latest local-runtime batch: the first active-turn queue slice now notices
  same-turn follow-ups/mail before finalizing assistant text, pauses stale tool
  calls when queued input appears, drains that input into model history with
  prompt-hook processing, and records the phase of each drain/pause. Local
  deterministic compaction now runs the same `PreCompact` / `PostCompact`
  command-hook lifecycle as model-assisted compaction. Hook command input uses
  the active turn id when available and includes child-agent identity/type for
  subagent hooks. OpenAI-compatible chat and Anthropic providers now use SSE
  streaming paths for text, thinking, tool-call, usage, and done events, while
  retaining JSON fallback for non-streaming responses. Read-only tool calls
  can now be pre-dispatched while the provider stream is still open, and stale
  preempted tool calls receive model-visible skipped outputs so follow-up turns
  do not inherit orphaned tool calls. Tool failures now carry an explicit
  recovery classification and unknown runtime failures default to fatal rather
  than being fed back to the model. Remaining high-impact runtime gaps are full
  streaming-time futures for mutating/interactive tools, full dynamic
  MCP/app/plugin tool inventory, exact `AgentControl`/mailbox semantics,
  event/item-graph history reconstruction, exact `TurnDiffTracker`, websocket
  ack/fallback, deeper local compaction/token lifecycle parity, and
  multi-environment tool routing.
- Latest verification: full Rust/Python/whitespace checks and Codex-auth root
  plus child smokes passed for the hook-metadata batch. Root session
  `1c902e5dc5eb` returned `Paris`, and child session `4f1a62227201` read
  `/tmp/but-codex-agent-parity-smoke.txt` and returned
  `agent-parity-smoke-ok`. The full Rust suite now covers 423 core tests; this
  loop also fixed a parallel test config-env leak by isolating `CODEX_HOME` in
  the model-compaction provider-summary test. A fresh ten-platform-auditor
  pass confirmed hook metadata is materially closed for command hooks and kept
  the same architectural gap set open.
- The next concrete runtime-tool slice closes several of those audit findings:
  Bash and apply_patch hooks now receive Codex-style `tool_input.command`
  payloads while preserving `raw_tool_input` for local diagnostics, and
  `updatedInput.command` is rewritten back into local `cmd`/patch arguments.
  Shell apply-patch rescue now also recognizes `applypatch`, single-argument
  direct invocation, and strict `cd <path> && apply_patch <<EOF` forms. Local
  image handling now normalizes/resizes before applying the inline byte limit,
  and memory summary context includes Codex-style read-path guidance about
  precedence, use boundaries, and avoiding direct memory-file edits.
- A fresh ten-agent audit after that slice found the obvious hook/applypatch/
  image mechanics closed and moved the concrete next targets to runtime
  recovery: invalid-image provider-error repair, compaction overflow
  drop-oldest-and-retry behavior, actually executing `async_run` hooks, and
  avoiding exact-looking `turn.diff` output when local only has a broad git
  snapshot. The larger remaining pillars are still dynamic tool routing,
  mutating streaming futures, typed history/rollout state, active-turn mailbox
  semantics, goal budget lifecycle, full hook discovery/trust/concurrency, and
  multi-environment tool routing.
- The current runtime-recovery slice closes those concrete targets. Providers
  now classify invalid-image failures distinctly; the provider loop repairs one
  rejected tool-output image by replacing it with `Invalid image` and retries
  the turn. Model-assisted local compaction retries context-window overflow by
  dropping oldest history from the summary request before falling back. Command
  hooks marked `async_run` execute inline for now, so their context/blocking
  effects are no longer skipped. Git snapshot `turn.diff` events now mark dirty
  baselines as inexact and omit unified diffs when unrelated preexisting work
  would be mixed into the report.
- Verification for that slice passed across full Rust workspace tests, Python
  tests, formatting, whitespace, and real Codex-auth root/child smokes. A fresh
  ten-subagent broad audit found no new small concrete recovery-class gaps; the
  remaining work is concentrated in larger runtime architecture: dynamic tool
  routing, streaming futures, live active-turn/AgentControl state, typed
  history/rollout reconstruction, goal accounting, hook concurrency/source
  metadata, exact `TurnDiffTracker`, and deeper local compaction/token
  lifecycle precision.
- The current hook/compaction slice closes a concrete part of that larger hook
  runtime gap. Matching command hooks now execute concurrently, preserving
  configured order for stable context/feedback while selecting
  `PreToolUse.updatedInput` by completion order like Codex. Hook lifecycle
  events record both configured and completion order. Model-compaction overflow
  retry now removes an old assistant/tool-output pair together when trimming
  summary-request history, reducing invalid call/output fragments in local
  compaction prompts.
- The current hook-metadata slice moves that same runtime surface closer to
  Codex's observable hook lifecycle. Hook events now include Codex-shaped run
  summaries with source path/kind, display order, sync/async mode, scope,
  status, timestamps, duration, output entries, state key, trusted hash, and
  trust status. Hook command stdin now receives a real transcript snapshot path,
  and `[hooks.state]` can disable configured hooks. Remaining hook-specific
  gaps are prompt/agent hook handlers, exact trust enforcement/listing UX,
  plugin/cloud hook discovery sources, strict output validation, and
  app-server notification surfaces.
- Verification for the hook-metadata slice passed with full Rust workspace
  tests, Python tests, formatting, whitespace, and real Codex-auth root/child
  smokes. Root session `1c902e5dc5eb` returned `Paris`; child session
  `4f1a62227201` read `/tmp/but-codex-agent-parity-smoke.txt` and returned
  `agent-parity-smoke-ok`. The follow-up audit reached ten broad platform
  reports and agreed the remaining high-impact gaps are architectural: dynamic
  tool routing/inventory, true streaming tool futures with cancellation and
  read/write gating, active-turn `InputQueue`/`AgentControl` semantics, typed
  history/rollout reconstruction, deeper local compaction/token lifecycle
  parity, subagent control-plane depth, exact turn diff tracking, generic
  extension/contributor runtime including memory tools, and goal runtime
  accounting. Hook-specific remaining gaps are now lower priority:
  prompt/agent hook handlers, exact trust UX/enforcement, plugin/cloud hook
  discovery, stricter output validation, app-server notifications, and one
  `SubagentStop` transcript-path null.
- The current memory slice replaces the local passive memory-summary block
  with Codex's read-path developer policy. Enabled memories now tell the model
  when to do a memory pass, how to search `MEMORY.md` and rollout summaries,
  how to handle stale memory, how to emit the hidden `<oai-mem-citation>`
  block, and how to write update notes only when explicitly asked. Source
  inspection also found that Codex's memory list/read/search tool contributor
  exists in the extension crate but is not installed in current Codex, so this
  loop deliberately did not add callable memory tools locally.
- Verification for the memory slice passed with full Rust workspace tests,
  Python tests, formatting, whitespace, and real Codex-auth root/child smokes.
  Root session `dbf87fd06c77` returned `Paris`; child session `2a94f1fc1a2e`
  read `/tmp/but-codex-agent-parity-smoke.txt` and returned
  `agent-parity-smoke-ok`.
- The follow-up ten-auditor pass agreed the memory read-path policy is closed
  and that not exposing memory tools matches current Codex. The dominant open
  work is still architectural: dynamic tool routing/inventory,
  stream-integrated tool futures, active-turn input/mailbox state, typed
  history/rollout/context-manager reconstruction, exact turn diffs, goal
  runtime accounting, local compaction/token-window lifecycle, and deeper
  skill/plugin/review task semantics. The pass also produced smaller next-slice
  candidates: stream-safe hidden-markup filtering, structured memory citation
  accounting, real child transcript paths for `SubagentStop`, and a prompt
  drift check for uncataloged model fallbacks.
- The current slice closes those concrete candidates. Streaming assistant text
  is filtered before `model.stream_delta` and live `model.delta`, including
  split partial hidden-tag openings. Final assistant text and `done.result`
  parse Codex-shaped memory citation blocks into `memory.citation` sidecars
  with entries plus deduped `rollout_ids`/legacy `thread_ids`. `SubagentStop`
  hook input now carries real child and parent transcript snapshot paths.
  Source inspection showed the local uncataloged Codex fallback prompt is
  already byte-identical to Codex `codex-rs/models-manager/prompt.md`, so the
  GPT-5.1-specific prompt item is not a current fallback-prompt gap.
- Verification for this slice passed with full Rust workspace tests, Python
  tests, formatting, whitespace, and real Codex-auth root/child smokes. Root
  session `fa75168e01bf` returned `Paris`; child session `abdafe765c1b` read
  `/tmp/but-codex-agent-parity-smoke.txt` and returned
  `agent-parity-smoke-ok`.
- The follow-up ten-auditor pass agreed this slice closed the previous
  lower-scope targets and did not find a new small blocker. The remaining work
  is architectural: dynamic router/inventory, stream-integrated mutating tool
  futures, active-turn input/control state, typed context/history/rollout
  reconstruction, exact turn diffs, full goal accounting, local
  compaction/token-window precision, skills/plugins manager depth, and
  review-task lifecycle. Two caveats remain: memory citation accounting is
  still sidecar/event-based rather than Codex's typed agent-message/state-db
  path, and truly unknown non-Codex models intentionally use the neutral
  fallback prompt for provider portability.
- Status: a 10-scope Codex-auth closure audit on 2026-05-24 found the gap is
  not closed. Prompt/context alignment is substantially improved, `apply_patch`
  has verified-write semantics for common Codex patch behavior, streaming
  progress/final lifecycle events, committed-prefix failure reporting, plus the
  first Codex-shaped writable-root and protected-metadata safety slice, and the
  shell command path now has a first Codex-shaped safety slice for read-only
  parallelization, destructive `rm -f`/`rm -rf` rejection before spawn, and
  Codex-shaped approval metadata, hidden additional-permission handling, and
  no-approval permission instructions. The read-only shell parser now rejects
  Codex parser edges such as unquoted comments, trailing shell operators,
  empty pipeline segments, redirection, substitutions, variable-assignment
  prefixes, and unsafe `base64`/`find`/`rg`/`git` flags, while keeping the
  deliberate local `/bin/rm -rf` hardening until a real approval UI exists.
  Unified `exec_command` and `write_stdin` results now use Codex-style
  model-visible text and numeric running-session ids while preserving this
  repo's structured JSON `tool.finished` payloads for TUI/state projection.
  Non-TTY command stdin is now closed like Codex, and non-empty
  `write_stdin` input is accepted only for TTY-backed sessions.
  Unified exec wait/output limits now match Codex's main runtime shape for the
  local tool layer: exec and non-empty write waits clamp to 30 seconds, empty
  polls can wait in the 5s-300s background range, and oversized
  `max_output_tokens` requests are capped by this repo's effective tool-output
  token policy. Per-call command output is now collected through a Codex-style
  1 MiB head/tail buffer instead of an unbounded vector, while a separate final
  transcript buffer preserves whole-process completion output for background
  `command.finished` events. Command readers now stream no-newline output
  chunks, so prompts and progress text are visible before EOF, and stdin write
  failures are included in model-visible recovery text. Running unified-exec
  sessions now survive normal turn completion and can be reused by follow-up
  `write_stdin` calls like Codex, while read-only shell classification uses
  Codex's tree-sitter-bash word-only parser shape for nested `bash -lc` and
  `zsh -lc` sequences, safe command separators, quoted strings, and
  concatenated literal words. Exec spawning now also resolves Codex-shaped shell
  types for Bash, Zsh, Sh, PowerShell, and Cmd, derives shell-specific argv,
  emits Codex-style six-hex chunk ids, and gives non-empty `write_stdin` input
  the same short reaction window before collecting output.
  Compaction now preserves the canonical
  startup context instead of replacing it with only a summary. AGENTS discovery
  now reads Codex's project-doc config knobs from Browser Use Terminal home
  `config.toml` and trusted project-local `.browser-use/config.toml`, including
  the final managed-config and macOS managed-preferences precedence layers.
  Codex-style `developer_instructions` are now loaded from that same config
  stack and appended to the aggregated developer context alongside the
  no-approval permissions instructions. Typed runtime `AgentRunOptions`
  `base_instructions` and `developer_instructions` now match Codex override
  precedence by beating config layers for model-visible prompt context while
  preserving the first persisted session base instructions for default resumes.
  Environment context now covers
  Codex-shaped network, multiple-environment, active-subagent, and disable
  semantics for the runtime facts this harness has. Config-backed
  `instructions` and `model_instructions_file` now override provider base
  instructions through the provider instruction channel. Config-backed
  `model_reasoning_effort`, `model_reasoning_summary`,
  `model_supports_reasoning_summaries`, and `model_verbosity` now flow into
  OpenAI Responses and Codex Responses request bodies through Codex-shaped
  bundled-model defaults and capability gates. Default model instructions now
  render Codex-style `none`, `friendly`, and `pragmatic` personality variants,
  with `pragmatic` as the default and the browser-use tool contract layered
  afterward. Sessions now persist the resolved base instructions and reuse that
  value on resume-like provider runs unless an explicit base-instruction
  override is supplied, matching Codex's `SessionMeta.base_instructions`
  behavior through this repo's event store. When the provider model changes
  while the session base is frozen, the runtime now injects Codex's developer
  `<model_switch>` context instead of rewriting the base prompt. When only the
  personality changes under the same model, the runtime now injects Codex's
  developer `<personality_spec>` context while keeping the base prompt frozen.
  Contextual workspace updates now preserve Codex-like placement: initial
  AGENTS/environment context is grouped before the first real prompt, while
  later `before_seq` environment updates stay next to the follow-up they target
  instead of being hoisted back to session start. The event store now also
  records a non-model-visible `context.baseline` once per real user turn and
  stores compacted replacement messages as durable replay checkpoints, matching
  Codex's core `TurnContextItem` and compaction-checkpoint heuristics in the
  parts this repo can represent without app-server APIs. Rollback markers are
  now replayed like Codex at the provider-history layer: rolled-back local user
  turns and their anchored contextual updates are removed from model input
  without physically deleting events. Previous model/personality/settings
  decisions now prefer the newest surviving `context.baseline` tied to a real
  local user turn, rather than raw transient `model.config` events. When a
  surviving session switches provider models, the current reasoning effort is
  normalized against the target model's supported efforts like Codex: supported
  efforts are preserved and unsupported or absent efforts use the middle
  supported/default effort. Multi-agent spawning now follows Codex v2 for the
  main model-visible contract, built-in role validation, configured and
  discovered user role files, role/requested/parent model settings,
  service-tier propagation for the modeled bundled models, role metadata
  validation, role-specific spawned-agent nicknames, and schema notes for role
  config files that lock model, reasoning effort, or service tier.
  `features.multi_agent_v2.hide_spawn_agent_metadata` now also matches Codex:
  when enabled, the model-visible schema omits spawn metadata override fields
  and the v2 tool result returns only `task_name` while retaining internal child
  nickname metadata. The visible `spawn_agent` description now also includes
  Codex-style bundled model override guidance with descriptions, reasoning
  effort options, default markers, and service tier ids for the first five
  picker-visible models. The remaining v2 management tools now match Codex's
  model-visible surface as well: no legacy `send_input`, Codex-shaped
  `send_message`, `followup_task`, `wait_agent`, `list_agents`, and
  `close_agent` schemas, compact wait/list/close outputs, and matching root
  target semantics for message, follow-up, and close behavior.
  Collaboration/Plan mode now includes the local product wiring needed for
  this terminal: persisted Default/Plan mode selection, `/mode`, `/plan`,
  Shift+Tab cycling when idle, CLI `--collaboration-mode`, runtime pass-through
  into `AgentRunOptions`, a request-answer path for `request_user_input`, active
  question transcript rows, rendered proposed-plan blocks, active-turn `/plan`
  safety, and Codex's Plan-mode medium reasoning preset. The TUI model picker
  now uses the same active catalog substrate as provider/runtime heuristics for
  Codex and OpenAI API model rows, and follow-ups/retries keep the selected
  session's provider/model instead of inheriting whichever global picker value
  is currently selected.

## Latest Batch

The latest batch implemented a broad local-agent-quality slice from the
current gap list.

- The model-visible tool surface now includes represented Codex-style goal
  tools: `get_goal`, `create_goal`, and `update_goal`.
- Workspace context now injects local skills discovered under
  `BROWSER_USE_TERMINAL_HOME/skills`, using a Codex-shaped
  `<skills_instructions>` block.
- The CLI now has represented `review` prompt entry points for uncommitted
  changes, base branches, commits, and custom review instructions.
- The CLI now has `user-shell`, which runs a user shell command and records the
  result as model-visible workspace context for later agent turns.
- Successful and partial `apply_patch` runs now emit `turn.diff` sidecars with
  changed files and committed delta metadata.
- Follow-up turns for spawned children now recover spawn-time role config
  overrides, reasoning effort, service tier, and model metadata where the local
  runner can represent them.
- Unified exec now bootstraps commands from a bounded per-thread shell snapshot
  so aliases/functions/exports from the user shell can be available without
  overriding Codex's command-environment contract.
- The local synthetic near-deadline model warning was removed. The fallback
  prompt was expanded for provider-neutral agent quality without Codex/OpenAI
  branding.
- A follow-up sweep after the ten-subagent audit tightened the low-risk misses:
  goal usage now starts from a creation-time token baseline and `create_goal`
  rejects any existing thread goal, `user-shell` now runs from the session cwd
  with Codex-like command env defaults and truncated model-visible history, and
  unambiguous `$SkillName` text mentions now materialize the matching
  `SKILL.md` body.
- The next local slice isolated review sessions with review-specific base
  instructions and disabled goal/multi-agent/web/image tools, added
  `features.goals` for represented goal-tool exposure, added local
  `compact_prompt`/compact-prompt-file overrides, records server model reroutes
  and unknown-model catalog fallback warnings, validates/caps local image
  inlining, removed unsupported `prevent_idle_sleep` beta advertising, and
  clarified plugin MCP wording so configured servers are not described as
  callable unless their tools are separately exposed.
- The latest local slices targeted model-input quality directly: project-scoped
  `.agents/skills`/`.browser-use/skills` discovery plus frontmatter descriptions,
  cwd-aware plain `$Skill` materialization, opt-in read-only memory summary
  context, active-goal context injection before provider turns, Codex-like local
  prompt-image resizing to 2048px bounds, and honest `tool_search` wording for
  MCP/app/plugin tools that are not actually exposed. The follow-up extended
  that into Codex-shaped hidden user `<goal_context>` continuation prompts,
  provider-loop auto-continuation while an active goal has not been completed or
  strictly blocked, `$BROWSER_USE_TERMINAL_HOME/.agents/skills`, plugin skill
  roots, frontmatter names/metadata, `openai.yaml` short descriptions,
  `skills.include_instructions`, `skills.bundled.enabled`, `[[skills.config]]`
  disable rules, and
  `[memories] use_memories`.
- The latest broad runtime slice then removed the browser-harness contract from
  non-browser terminal prompts, added model-assisted local compaction behind a
  portable provider path, added command hook support for `SessionStart`,
  `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`,
  `PostCompact`, `Stop`, `SubagentStart`, and `SubagentStop`, expanded
  read-only parallel dispatch, records active-turn mailbox drain and git diff
  sidecars, and tightened hook semantics with Codex-style exact/regex matchers
  plus `PostToolUse` feedback replacing model-visible tool output.
- Verification passed: `cargo fmt --check`, `cargo test`,
  `uv run --with pytest python -m pytest -q`, and `git diff --check`. The
  standing Codex-auth smoke passed again in
  `/tmp/but-codex-smoke-agent-quality6.qYruta`: root `30942471de10` returned
  `Paris`, and child `7f840adea56e` read
  `/tmp/but-codex-agent-parity-smoke.txt` and returned
  `agent-parity-smoke-ok`.
- Ten read-only broad auditors then rechecked the current branch against Codex.
  They agreed the remaining high-impact gaps are now architectural: active-turn
  input queue semantics, streaming-time tool execution, Codex's full dynamic
  tool router/inventory, exact local compaction lifecycle, exact subagent
  control-plane semantics, exact history/turn-diff reconstruction, and sharper
  tool-error taxonomy. Hook parity is no longer absent, but remains incomplete
  around async/prompt/agent handlers, exact payload provenance, concurrent
  execution, strict output validation, and plugin hook sources.
- Remaining high-impact gaps are true websocket ack/fallback transport,
  dynamic MCP/app/deferred inventory surfaces, deeper goal budget/accounting
  edge cases, remaining full skill-manager semantics, exact local compaction
  lifecycle, streaming tool scheduling, input queue/active-turn steering, memory
  read-path tools/policy, full effective-config provenance, exact subagent
  control-plane behavior, and any app-server lifecycle surfaces that materially
  affect local agent quality.
- Final verification for this slice passed: full Rust workspace, Python tests,
  `git diff --check`, `scripts/verify-terminal-ui.sh`, artifact inspection under
  `/tmp/but-design-loop`, and real Codex-auth root/child smokes. Root
  `de3979b2a0c3` returned `Paris`; child `af40b1adc459` read
  `/tmp/but-codex-agent-parity-smoke.txt` and returned
  `agent-parity-smoke-ok`.

The previous batch extended the G-031/G-034 compaction/token lifecycle slice,
removed remote-compaction advertising, and removed Codex-only AWS/Bedrock
provider baggage.

The previous batch closed the next represented G-033/G-032 plugin/app mention
context slice after four read-only audits compared Codex plugin/app mention
parsing, effective config, websocket transport, and compaction/token surfaces.

- Core now parses the represented Codex plugin config knobs: `features.plugins`
  and `[plugins."<plugin>@<marketplace>"].enabled`. Enabled local plugin bundles
  are summarized from Codex-home cache/curated plugin roots, including manifest
  descriptions, skill presence, MCP server names, and app connector ids.
- Initial developer context can now include a Codex-style
  `<plugins_instructions>` block when enabled plugins are available. Explicit
  `plugin://` mentions can also materialize developer context for that turn,
  telling the model which plugin-associated skills, MCP servers, and apps exist.
- Linked mention parsing now follows Codex sigils for the represented text-link
  path: `$` for `skill://` and `app://`, `@` for `plugin://`. Wrong-sigil links
  remain ordinary text instead of silently creating typed mention sidecars.
- Skill bodies are now scanned for linked `app://` mentions, so a selected skill
  can add the same model-silent `app_connector_ids` sidecar Codex uses to choose
  app context. Top-level core, CLI, TUI, child-spawn, and follow-up paths use the
  cwd-aware materializer so persisted `session.input` and `session.followup`
  events carry stable mention context.
- Remaining plugin/app gaps are still product wiring rather than closed parity:
  full app connector auth/selection, MCP tool exposure from apps, remote plugin
  install/discovery/sync, plugin inventory UI, plugin provenance inside actual
  callable tool descriptions, and cloud/admin app requirements. The next
  high-impact non-plugin candidates remain effective-config provenance,
  websocket transport, and compaction/token baseline nuances.
- Focused core/CLI/TUI tests passed, then `scripts/verify-terminal-ui.sh`
  passed. I inspected `/tmp/but-design-loop` captures including running,
  developer, stopped/cancelled, bracketed paste, and completed-output; ANSI and
  bracketed-paste scans had no matches. The standing Codex-auth smoke passed in
  `/tmp/but-codex-smoke-plugin-mentions.8UyZDQ`: root `bae7b7066d41` returned
  `Paris`, and child `d9c611efb9b9` returned `agent-parity-smoke-ok`.

The previous batch closed the next G-028 unified-exec completion/cancellation
slice and documented the parser issue that the standing Codex-auth smoke found.

- The Codex-auth transport failure was a parser-classification bug, not a
  browser/tool problem: the ChatGPT Codex backend returned a valid SSE body
  without a `Content-Type` header, and local had routed missing content type to
  whole-body JSON parsing. The fix is Codex-shaped: only explicit
  `application/json` responses use the JSON parser; headerless streamed bodies
  remain on the SSE path.
- Unified exec now uses a Codex-style bounded post-exit reader drain instead of
  joining reader threads indefinitely. Completed processes get a 50 ms drain
  window, so background descendants holding stdout/stderr open cannot hang a
  turn forever.
- Background completion now waits a 100 ms trailing-output grace period before
  finalizing and emits any trailing `command.output` before `command.finished`.
  If descendants still hold the pipe open after the grace window, local detaches
  the old buffers so late descendant output does not leak into later polls.
- Normal `session.done` still preserves running `exec_command` sessions for
  follow-up `write_stdin`, but cancellation/stop paths now clean up the current
  agent subtree's unified-exec processes. This is wired through core finalization,
  CLI `cancel`, and TUI stop.
- The remaining non-exact G-028 pieces are pause-aware waits, because this
  terminal has no out-of-band pause signal yet; here-doc command-prefix metadata,
  which mostly feeds approval/rule logic we are intentionally ignoring; optional
  zsh-fork and deeper non-Unix policy/parser behavior; explicit reader
  cancellation internals; and app-server/alternate entry-point shutdown wiring
  if those surfaces are added later.
- Focused command/core/CLI/TUI tests passed, then `scripts/verify-terminal-ui.sh`
  passed. I inspected `/tmp/but-design-loop` captures including running,
  developer, stopped/cancelled, bracketed paste, and completed-output; ANSI and
  bracketed-paste scans had no matches. The standing Codex-auth smoke passed in
  `/tmp/but-codex-smoke-exec-drain.74XsF2`: root `8b7fbd1dc723` returned
  `Paris`, and child `7bc0c692348f` returned `agent-parity-smoke-ok`.

The previous batch closed the next bounded G-030 collaboration slice and fixed one
local call-id adaptation that was still stronger than Codex's turn-id behavior.

- `features.default_mode_request_user_input` is now parsed from the Codex-style
  feature layer. When enabled, the model-visible `request_user_input` tool
  description says it is available in Default or Plan mode, and runtime dispatch
  allows Default-mode root requests. The default remains Plan-only.
- `request_user_input.requested` events now carry the active Codex turn id when
  a turn lifecycle event is open, falling back to the call id only for older
  event streams without lifecycle metadata.
- Request-input response matching now prefers `turn_id` over `call_id`: a
  stale response with the same call id but a different turn id is ignored, while
  a matching turn-id response is accepted even if legacy call-id metadata differs.
- The TUI pending-request state and answer payloads now carry `turn_id`; the
  composer submits by turn id while preserving call-id fallback for older
  persisted requests.
- Focused core/TUI request-input tests passed, `scripts/verify-terminal-ui.sh`
  passed, `/tmp/but-design-loop` captures were inspected, ANSI/bracketed-paste
  scans had no matches, and the standing Codex-auth smoke passed in
  `/tmp/but-codex-smoke-request-input.OZh9L1`: root `77b45dbc6d6d` returned
  `Paris`, and child `b1b154645669` returned `agent-parity-smoke-ok`.

The batch immediately before this extended the hosted-tool/runtime cleanup
cluster across G-028, G-026, G-031, and G-032.

- Completed `image_generation_call` response items now save standard base64
  image bytes under the session artifact root in `generated_images/*.png`,
  record an `artifact.created` image artifact, and add Codex-style developer
  context telling the model where generated images are saved and to copy rather
  than move/delete the original unless asked. Failed decodes are non-fatal and
  do not inject the context, matching Codex.
- Immediate `end_turn=false` continuations see that generated-image developer
  context before the raw hosted image item, so follow-up model turns can refer
  to the saved file path without guessing.
- Hosted web search now resolves the default/cached preference to live search
  under this runtime's no-sandbox/danger-full-access permission posture, while
  explicit `web_search = "disabled"` still removes the hosted tool. The
  represented Codex `allowed_web_search_modes` constraint is now honored as
  well: cached-only requirements keep hosted search cached, live falls back to
  cached when disallowed, and disabled-only/empty requirements remove the tool.
- The provider parser now has a regression that inbound `response.processed`
  stream events are ignored and do not count as completion; real completion
  still requires `response.completed`, which matches Codex's websocket-only
  client-to-server `response.processed` semantics. A second regression proves
  `response.processed` by itself still fails as an incomplete stream.
- Model-turn request telemetry now records hosted tool counts and fingerprints
  separately, so the state/debug record reflects hosted `web_search` and
  `image_generation` as model-visible tools.
- Time-to-first-token now follows Codex's broader response-event heuristic:
  first hosted/tool response items, function/custom/tool-search calls, and
  non-empty reasoning/message response items can set TTFT, not only streamed
  text deltas.
- Unified-exec cleanup now has a whole-registry API and close-agent cleanup
  removes running commands for the target agent plus descendants. CLI
  `close-agent` uses the same subtree cleanup path, and both CLI and TUI
  binaries now install a shutdown guard that terminates all registered unified
  exec processes when the product exits. Normal turn completion still leaves
  running commands alive for follow-up parity.
- Focused tests, full terminal verification, and the standing Codex-auth
  root/subagent smoke passed. The latest smoke state was
  `/tmp/but-codex-smoke-parity.ZXeC1P`: root returned `Paris`, and the child
  read `/tmp/but-codex-agent-parity-smoke.txt` and returned
  `agent-parity-smoke-ok`.

The batch immediately before this closed the next runtime/history cluster
across G-028, G-031, and G-033 after the prior shell/hosted-tool audits. It
also fixed a real Codex-auth subagent smoke blocker that was introduced by
namespace replay drift.

- Legacy `shell_command` is no longer a wrapper over unified `exec_command`.
  It now runs as a one-shot classic shell command with no persistent session,
  hard `timeout_ms`/`timeout` handling, Codex-style `Exit code`/`Wall time`/
  `Output` model text, `session_id:null` command events, classic
  `tool.started`/`tool.finished` names, and config-gated login shell behavior.
- Visible shell handlers now follow Codex parallel-call capability, while
  hidden legacy `shell_command` compatibility dispatch remains serial.
- Hosted/raw Responses items now survive the two places where local history was
  dropping them: immediate `end_turn=false` continuations keep reasoning,
  web-search, and image-generation response items in the next request, and
  response-item compaction checkpoints retain hosted web/image calls instead of
  stripping them.
- Responses input serialization now preserves `namespace` on assistant tool
  calls. The Codex-auth subagent smoke previously failed with a 400 because a
  `spawn_agent` call was replayed without its namespace; after the fix, the
  smoke completed with real `spawn_agent`, child completion, `wait_agent`, and
  final `Paris`.
- At that point, remaining debt in these touched areas was narrower: exact
  web-search permission profile constraints, full generated-image artifact/UI extraction,
  pause-aware unified-exec waits/output notifications, optional zsh-fork and
  deeper non-Unix shell policy, and broader app-server lifecycle/config
  surfaces.

The batch immediately before this took the G-032 tool-capability cluster after
the standing Codex-auth root/subagent smoke passed. It closed the most direct
model-visible gaps in shell-family selection and hosted Responses tool
exposure, then fixed a G-033 observability bug caught by the same smoke gate.

- Stable Codex feature keys now affect local planning: `features.shell_tool`,
  `features.unified_exec`, legacy `experimental_use_unified_exec_tool`,
  `features.image_generation`, top-level `web_search`, deprecated
  `features.web_search_cached` / `features.web_search_request`, and
  `[tools.web_search]` settings.
- Model catalog metadata now preserves `shell_type`, `web_search_tool_type`,
  and `experimental_supported_tools` from Codex model rows.
- Shell planning now matches the Codex family selector for represented modes:
  `shell_tool = false` hides shell tools, `unified_exec = false` exposes legacy
  `shell_command`, and unified mode exposes `exec_command`/`write_stdin` while
  keeping `shell_command` registered for hidden legacy dispatch.
- Parallel dispatch now uses the same config-aware registry as serial dispatch,
  so disabled shell tools cannot bypass the feature gate through the read-only
  parallel path.
- Responses requests now carry hosted `web_search` and `image_generation` as
  first-class provider-turn specs with provider/model/config gates, instead of
  forcing those hosted tools through the ordinary function-tool schema.
- Unknown tool calls now return Codex-style `unsupported call: <tool>`.
- Session display/status/result paths now use only the session's own
  `session.done` payload. A late child `agent.completed` event still appears as
  subagent activity/mail, but it no longer overwrites a completed parent result
  in CLI `show`, local agent status, dataset summaries, or telemetry.

The batch immediately before this added a represented Codex
effective-config/session-thread-config layer plus permission/feature gating
behavior that directly affects model-visible context and tool choice. A
mandatory Codex-auth smoke then exposed a G-026 provider blocker, which was
fixed in the same loop: the ChatGPT Codex backend can return a valid SSE body
without a `Content-Type` header, so the local parser now treats only explicit
`application/json` responses as whole JSON and otherwise stays on the SSE path.

- `AgentRunOptions` now accepts a session-thread-config layer that mirrors
  Codex `SessionThreadConfig`: `model_provider`, `model_providers`, and boolean
  `features`.
- That layer is applied after ordinary request/session config overrides and
  before managed config, matching Codex equal-precedence insertion order. It now
  drives provider selection, beta-feature headers, model request metadata, tool
  schemas, base/personality context, and snapshot invalidation consistently.
- `include_permissions_instructions = false` now suppresses only the default
  permissions block. Runtime developer instructions and collaboration-mode
  instructions still reach the model, and a non-model-visible suppression marker
  prevents compaction from reintroducing default permissions later.
- Runtime options can also suppress permissions instructions for the current
  run.
- `features.multi_agent = false` now disables the multi-agent tool family,
  including deferred v1 tool search, while leaving the rest of the terminal tool
  surface intact.
- The Codex-auth health smoke is now documented as a standing gate. After the
  provider fix, the root smoke completed with `Paris`, and the spawned child
  smoke read `/tmp/but-codex-subagent-smoke.txt` and completed with
  `subagent-smoke-ok`.
- Focused tests cover thread-config precedence over request overrides,
  managed-config precedence over thread config, permission suppression with
  developer/collaboration preservation, compaction replay, multi-agent gating,
  and headerless Codex SSE parsing. Full Rust/Python verification and the
  Codex-auth root/subagent smokes passed after the provider fix.

The previous batch intentionally took a larger step and closed two separate
agent-quality clusters: G-033 typed input production/replay across top-level
CLI/TUI/multi-agent paths, and the next G-028 unified-exec completion/drain
contract.

- Core now owns reusable typed user-input payload builders. Top-level text can
  materialize explicit linked references such as `[$Calendar](app://calendar)`,
  `[@Notes](plugin://notes@example)`, and `[$Docs](skill:///.../SKILL.md)`.
- CLI `start`, CLI `followup`, CLI `spawn-agent` child input, TUI start, and
  TUI follow-up now use the same typed payload builder instead of plain
  `{"text": ...}` events.
- `skill` references still materialize stable `<skill>` context at event
  creation time. `app://` and `plugin://` references are persisted as sidecars
  (`app_connector_ids` and `plugin_mentions`) and are not replayed as ordinary
  prompt text. Plugin developer context is replayed only when already
  materialized in the event payload, avoiding invented plugin capabilities.
- The v1 structured input schema now exposes the `detail` field that the
  runtime already honored for `image` and `local_image` items.
- Unified exec now uses one shared `command.finished` emitter for immediate,
  polled, and background completion paths. Background exit watchers emit unread
  trailing `command.output` before `command.finished`, completed
  `write_stdin` responses return `session_id: null`, write-after-exit does not
  surface stale write errors, and explicit child-agent close paths clean up
  that child's background commands without restoring normal-turn cleanup.
- Focused tests cover typed top-level links, materialized plugin context replay,
  v1 typed input, CLI/TUI typed payload production, v1 image-detail schema,
  command completion/session-id/write-after-exit cleanup behavior, close-agent
  command cleanup, and the full command module.
- Full terminal verification passed through `scripts/verify-terminal-ui.sh`,
  including Rust/Python suites, deterministic dumps, real tmux terminal smoke,
  and artifact inspection under `/tmp/but-design-loop`.
- Read-only subagent audits from that batch kept the remaining G-032
  provenance/product surfaces open at the time; the current batch closed the
  hosted-tool and shell-feature-gate portion, while app-server thread-config
  protocol and effective-config display/provenance remain open.

The batch before that closed two model-visible parity slices: G-032 typed runtime
instruction overrides and a G-028 unified-exec response/shell fidelity cluster.

- `AgentRunOptions` now has typed `base_instructions` and
  `developer_instructions` overrides. Runtime base instructions beat config
  `instructions` and persisted session base for the current provider turn, but
  only write the durable `session.base_instructions` event when the session has
  no prior base, matching Codex's session-meta behavior.
- Runtime developer instructions now beat user and managed config
  `developer_instructions` in the initial developer context bundle.
- Unified exec now has a local Codex-shaped shell substrate: detected
  `ShellType` values for Bash, Zsh, PowerShell, Sh, and Cmd; Codex-style
  fallback selection; `-lc`/`-c`, PowerShell `-NoProfile -Command`, and Cmd
  `/c` argv derivation; and `command.started.shell_type` state for auditability.
- `exec_command` and `write_stdin` model text now use Codex-style random
  six-hex chunk ids instead of deterministic call-id-derived chunk names.
- Non-empty `write_stdin` calls now sleep for the Codex 100 ms process reaction
  window before polling output, reducing missed interactive responses.
- Focused tests cover runtime base/developer override precedence, shell
  detection and argv derivation, Codex chunk-id shape, command output
  truncation, the PTY stdin reaction window, and the full command tool module.
- Read-only follow-up audits kept the then-remaining high-level G-032 and G-033
  gaps open: full `ConfigLayerStack`/effective-config provenance and typed
  plugin/app mention context needed larger product substrate work. A later plugin
  slice closed the represented mention-context subset, but not full app/plugin
  product wiring.

The previous batch closed the high-impact G-028 unified-exec slice:
Codex-style process lifetime across completed turns plus the represented
tree-sitter shell parser path for safe read-only command classification.

- Normal run completion no longer calls unified-exec cleanup. Background
  `exec_command` sessions stay session-scoped, matching Codex's process manager
  behavior for follow-up turns.
- A provider-level regression now starts a TTY-backed background command,
  completes the turn, appends a follow-up, and verifies `write_stdin` can reuse
  the same numeric `session_id` and receive output from the same process.
- The old hand-written rough shell parser was replaced with the Codex
  tree-sitter-bash word-only sequence parser shape for the represented Unix
  path. It accepts plain commands joined by `&&`, `||`, `;`, and `|`, quoted
  literal strings, numbers, and literal concatenations such as `-g"*.rs"`.
- The parser now rejects the same high-value unsafe/malformed constructs as the
  Codex parser for this path: comments, redirection, substitutions, variable
  assignments, trailing operators, leading operators, double separators, and
  empty pipeline segments.
- Read-only shell-wrapper detection now benefits from that parser for nested
  `bash -lc`, `zsh -lc`, and `sh -c` command sequences before applying the
  existing Codex-shaped read-only/dangerous-command classification.
- Three read-only subagents also re-audited related G-026/G-031 WebSocket,
  G-028 process/parser, and G-032 config-stack gaps. WebSocket transport,
  `response.processed`, sticky HTTP fallback, and full effective-config
  provenance remain open follow-up clusters rather than this turn's runtime
  slice.
- Remaining G-028 gaps are optional zsh-fork behavior, deeper non-Unix shell
  policy/parser semantics beyond argv derivation, pause-aware deadline extension,
  notification-based
  output waiting/post-exit drain exactness, here-doc command-prefix metadata,
  code-mode JSON output if this harness adds code mode, and explicit
  shutdown/stop wiring for background command cleanup. Approval logic stays
  intentionally out of scope for this experiment.

The previous batch closed a high-impact G-028/G-029 tool-runtime parity slice:
command output fidelity for interactive processes and Codex-shaped patch
progress/final lifecycle reporting.

The previous batch closed the represented MultiAgentV1 skill/reference/resume
cluster in G-033.

The previous batch closed the represented MultiAgentV1 typed-input and deferred
tool-discovery cluster in G-033.

The previous batch closed the represented multi-agent v2 config, mailbox,
completion, concurrency, and usage-hint cluster in G-033.

The previous batch before that closed the Bedrock AWS SigV4/profile-auth
execution slice in G-032. That slice has since been removed because it was
Codex product/provider baggage, not portable agent-quality behavior for this
terminal.

The previous batch closed the TUI runtime config propagation gap in G-032. The
terminal now accepts Codex-style `--profile/-p` and `--config/-c key=value`, and
those config layers affect the same agent-quality surfaces as the CLI path.

The previous batch closed the represented provider-object resume drift in G-032.
Local now persists a Codex-shaped provider definition in `session.config_snapshot`
and rebuilds resumed OpenAI/Codex-compatible providers from that snapshot when a
turn is resumed with default model/provider selection.

The previous G-032 batch closed the model-visible bundled prompt/catalog drift.
Local now vendors Codex's active `models-manager/models.json` and fallback
`prompt.md` byte-for-byte, renders bundled model instructions through Codex
`ModelMessages` semantics, and appends only this product's browser contract
after the Codex prompt prefix.

The previous G-032 batch added the first core-owned Codex-style session
configuration snapshot. Local persists the resolved model/provider id, request
settings, request metadata, retry/stream settings, and collaboration mode in
`session.config_snapshot`, then reuses that snapshot when defaults drift between
turns unless the next run explicitly overrides model/provider/reasoning.

The previous G-032 batch closed the represented Codex model-driven tool-output
truncation slice. That directly affects how much command, Python, browser,
file, and generic tool text the model sees after a tool call.

The previous G-032 batch moved the model catalog from a bundled static lookup
into runtime behavior. It added an owned `ModelCatalog`, `model_catalog_json`,
fresh cache loading, provider-turn metadata propagation, `view_image` gating,
spawn-agent model/reasoning validation, and service-tier validation.

The previous provider/model batch targeted provider-registry request parity.
Codex's provider registry and model catalog both affect the request the model sees:
base URL, auth source, headers, query parameters, retry settings, stream timeout,
selected `model_provider`, and model capability flags such as
`parallel_tool_calls`.

- Ran three read-only Codex/local audits focused on `ModelProviderInfo`, config
  merge behavior, `ModelsManager`/`ModelInfo`, and the local request-building
  insertion points.
- Added a structured Codex-shaped custom provider slice for `model_provider` and
  `model_providers.<id>`: `name`, `base_url`, `env_key`,
  `env_key_instructions`, `experimental_bearer_token`, `wire_api`,
  `query_params`, `http_headers`, `env_http_headers`, retry/stream settings,
  and websocket/auth metadata.
- Wired normal config-backed OpenAI/Codex runs so a selected custom provider can
  construct an OpenAI Responses HTTP provider with the configured provider id,
  base URL, auth token, query params, and headers. Missing environment-backed
  headers are skipped like Codex.
- Updated provider request metadata to inject provider-registry query params and
  static/env headers.
- Updated the bundled GPT/Codex model slice so `gpt-5.5`, `gpt-5.4`,
  `gpt-5.4-mini`, `gpt-5.3-codex`, `gpt-5.2`, and `codex-auto-review` send
  `parallel_tool_calls:true`, matching Codex's current bundled catalog.
- Full runtime verification passed with formatting, Rust tests, Python tests,
  and whitespace checks. No TUI renderer, keyboard, terminal-output, or
  scrollback paths changed, so the terminal UI verifier was not rerun.
- Remaining provider/model gaps are command-backed auth execution and 401
  refresh retry, full built-in provider merge semantics,
  thread-config lock/persistence, remote/cache `/models` plus ETag refresh,
  exact dynamic catalog prompt metadata, and websocket transport/fallback.

Earlier recent batches continued G-031/G-034 and moved local
history/finalization toward Codex's response-item model. Codex treats typed
`ResponseItem` rollout items as canonical model history; local had been
reconstructing from deltas, tool-call events, `tool.finished`, and
`session.done`.

- Ran two read-only Codex/local audits focused on `ResponseItem`, rollout
  reconstruction, finalized assistant-message extraction, `previous_response_id`,
  `response.processed`, and websocket fallback. Both audits said to make response
  output items first-class before attempting automatic incremental resume.
- Added `ModelEvent::ResponseOutputItem`, provider parsing for SSE
  `response.output_item.done` and non-stream Responses `output`, and hidden
  `model.response.output_item` persistence in core.
- De-duped raw output items repeated in `response.completed.output`, while
  keeping existing streamed delta/tool-call behavior for compatibility.
- Fixed the non-stream Responses path to preserve `response.id` and `end_turn`
  through `ResponseCompleted`.
- Replayed assistant messages and tool calls from persisted response items when
  available, de-duping the older `model.delta`/`model.tool_call` trail.
- Added hidden `model.response.input_item` persistence for local tool outputs,
  using Codex-shaped `function_call_output` and `custom_tool_call_output`
  payloads. Replay now prefers exact persisted tool-output content over
  synthetic `tool.finished` summaries when both exist.
- Added `replacement_response_items` to compaction checkpoints beside
  `replacement_messages`, so compacted history has response-item metadata for
  represented assistant/tool-call/tool-output state instead of only provider
  message JSON.
- Made text-only finalization and `task_complete.last_agent_message` prefer
  `phase:"final_answer"` response items, so commentary and final answer phases
  do not collapse into the terminal result when providers supply phase metadata.
- Added explicit `previous_response_id` request-field plumbing for OpenAI and
  Codex Responses request builders. Later provider transport audits corrected
  the transport placement: Codex automatic previous-response reuse is
  websocket-only, so local now keeps explicit `previous_response_id`
  serialization but sends full HTTP history unless and until websocket transport
  exists.
- Focused verification passed for provider response parsing/request fields,
  provider loop finalization, provider-message replay, exact persisted
  tool-output replay, update-plan output item recording, and compaction
  response-item checkpointing. Full runtime verification also passed with
  formatting, Rust tests, Python tests, and whitespace checks.
- Two read-only verifiers checked the result against Codex source. They agreed
  the new substrate is aligned directionally, and that the gap is still open in
  two large clusters: typed rollout/history reconstruction and stateful
  Responses transport/retry behavior.
- Continued the Responses transport/retry slice by typing local `response.failed`
  errors. Context-window, quota, usage, invalid-prompt, cyber-policy,
  server-overloaded, and rate-limit failures now carry Codex-like retryability
  hints instead of relying only on string matching.
- Parsed rate-limit "try again in ..." delays from `response.failed` messages
  and used them for `model.turn.retry.delay_ms`, matching Codex's requested-delay
  preference before exponential backoff.
- Treated `server_is_overloaded` and `slow_down` as terminal provider failures,
  removing the earlier broad "overloaded" transient heuristic.
- Made Responses `response.completed` parsing require an `id` field, matching
  Codex's typed `ResponseCompleted { id: String }` deserialization behavior for
  SSE and JSON Responses output. Local currently also rejects an empty string,
  which is slightly stricter than Codex's field-level requirement.
- A read-only verifier confirmed the retry/error slice is directionally aligned
  and still partial. The remaining provider-error work at that point was full
  `ApiError`/`CodexErr` coverage, HTTP/send/body typing, idle-timeout behavior,
  Codex's latched `response.failed` semantics, hidden request retry policy,
  websocket fallback, and `codex_error_info` payloads.
- Continued the same provider-error slice with three read-only audits against
  Codex. Local now maps non-success HTTP failures like Codex for 400 cyber/invalid,
  429 usage/retry-limit, 500 internal-server, and 503 server-overloaded cases;
  treats JSON body decode failures as retryable; skips malformed SSE frames; emits
  Codex-shaped `stream_error.codex_error_info` for typed provider failures; and
  no longer reactive-compacts a normal sampling `ContextWindowExceeded`, matching
  Codex's terminal behavior for that path.
- Added Codex-style hidden request retries inside provider send paths. OpenAI
  Responses, Codex Responses, OpenAI-compatible chat, and Anthropic now use the
  Codex default of 4 hidden retries, 200ms jittered exponential backoff, retry
  5xx and transport send/read failures, and do not retry 429 by default.
- Follow-up retry/config parity now threads the selected Codex-style
  `model_provider_id` through local turns, applies
  `model_providers.<id>.request_max_retries` with Codex's 100 cap, rejects
  reserved or unnamed custom provider tables for this config path, and keeps
  reqwest builder errors terminal instead of retrying them as transport errors.
  A later provider-registry slice now parses Codex-shaped custom provider
  fields and uses them for HTTP Responses request construction: selected
  `model_provider`, custom `base_url`, `env_key`, experimental bearer token,
  query params, static/env headers, retry caps, stream timeout, and
  `wire_api = "responses"` validation. Command-backed auth, full built-in merge
  semantics, websocket fields, and
  thread-config persistence remain open.
- Added the Codex response-header/rate-limit metadata slice. Local Responses
  streams now emit server-model, rate-limit, models-etag, and
  server-reasoning-included events from headers before SSE body events; 429
  `usage_limit_reached` errors preserve headers, carry the active rate-limit
  snapshot, and use Codex-style display copy; core stores the latest
  `model.rate_limits` snapshot and includes it in `token_count.rate_limits`,
  including usage-limit terminal events with `info:null`.
- Added Codex SSE metadata parity for server-model reroutes and model
  verification recommendations. Local now reads `response.headers` and top-level
  `headers` from stream events, dedupes repeated server-model values, maps
  `response.metadata.openai_verification_recommendation` to
  `trusted_access_for_cyber`, and records only the first model-verification
  notification for a turn.
- A read-only verifier for previous-response behavior confirmed the core
  heuristic matches Codex's `get_incremental_items` rule, while leaving the
  transport placement open: Codex automatic reuse is websocket-only and sends
  `response.processed`; local now keeps explicit `previous_response_id`
  serialization but sends full HTTP history unless/until websocket transport
  exists.
- Added Codex-style active-context token recomputation after represented history
  rewrites. Compaction now emits a recomputed `token_count`, resumed local runs
  repair missing recompute state when the latest `session.compacted` or
  `session.rollback` is newer than the latest `token_count`, consumed
  `total_token_usage` stays cumulative, and recomputed active context is stored
  only in `last_token_usage.total_tokens` with the other breakdown fields zero.
  A read-only Codex audit confirmed this field ownership and noted the remaining
  exactness gap: local estimates over provider-message JSON rather than Codex's
  typed `ResponseItem` history.
- Started the typed rollout/history reconstruction cluster by making compaction
  replay prefer `replacement_response_items` when present. Local still projects
  those response items back into provider messages for the current provider
  abstraction, but the replay checkpoint source now matches Codex's
  `CompactedItem { replacement_history }` direction.
- Added rollback coverage over response-item-backed compaction checkpoints, so
  local rollback can drop user turns reconstructed from the response-item
  replacement history rather than only from legacy `replacement_messages`.
- Extended normal replay to preserve unprojected Codex API-history response
  items instead of dropping them: `reasoning`, `local_shell_call`,
  `tool_search_call`/`tool_search_output`, `web_search_call`,
  `image_generation_call`, `compaction`, and `context_compaction` can now survive
  replay and pass through the Responses request input path.
- Added Codex-like request sanitization for raw response items: local strips
  transient `id` fields except for image-generation items and omits raw
  `reasoning_text` content while preserving encrypted reasoning payloads.
- Kept fallback chat/Anthropic providers from misreading raw Responses-only
  history items as empty user messages.
- A follow-up Codex source audit clarified the retention split: normal
  prompt/resume history keeps these API items, while Codex compaction
  replacement history keeps only messages plus `compaction` and
  `context_compaction` among raw non-message items. Local now applies that raw
  non-message filter when writing new replacement-response checkpoints, while
  still preserving older checkpoints that already contain broader raw items.
- Added the first Codex-style typed-history normalization before Responses
  request serialization: missing `function_call`, `custom_tool_call`,
  `local_shell_call`, and `tool_search_call` outputs are synthesized as aborted
  or empty completed outputs, and orphan raw call outputs are removed except for
  server/no-call-id tool-search outputs.
- Added Codex-style unsupported-image normalization for Responses requests:
  text-only targets receive the fixed image-omitted placeholder in place of
  `input_image` content, and `image_generation_call.result` is cleared. Bundled
  and unknown local model metadata keep image input enabled, matching Codex's
  current bundled catalog and omitted-modality default.
- Rebuilt local compaction checkpoints in Codex's local-summary shape: retained
  real user messages plus one Codex-prefixed summary user message, with current
  initial context reinserted at the Codex boundary and pending/recent tool
  artifacts removed from `replacement_response_items`.
- Added rollback coverage for response-item checkpoints with a compaction
  summary, so a rolled-back post-compaction follow-up is removed while the
  preserved summary remains available to the next turn.
- Ran fork-history audits in parallel. They confirmed the remaining fork gap is
  not browser-specific: Codex forks filtered rollout items, while local
  `spawn_agent` still inherits a compact sanitized JSON context for `all` and
  `last_n`.
- Added the first real fork-history implementation for `spawn_agent`: full and
  numeric last-N forks now seed filtered response-item history, preserve only
  Codex-retained message items plus assistant `final_answer`, drop tool/search/
  reasoning/image/commentary artifacts, and suppress the old inherited compact
  JSON in the child prompt. Full-history forks also avoid fresh workspace
  context reinjection to reduce duplicated prompt-prefix context.
- Added Codex-style inter-agent boundaries for local spawn forks: fork-only
  replay synthesizes assistant inter-agent messages from parent `agent.message`
  events, requires the full inter-agent envelope shape for trigger boundaries,
  counts `trigger_turn=true` as a last-N boundary, counts all inter-agent
  instructions as fork-rollback turns, then filters those mailbox messages back
  out of the child model-visible history.

Earlier batches focused on prompt assembly and project instructions because
those affect nearly every downstream agent decision. The second batch tightened
AGENTS.md parity.

- Added a Codex-style agent-quality contract for autonomy, planning, tool use,
  editing safety, git hygiene, verification, and final responses.
- Kept the existing browser-use/browser-harness contract after that layer so
  browser wiring remains unchanged.
- Reused the provider prompt assembly for compaction so resumed/compacted turns
  restate the same contract.
- Added AGENTS.md discovery and `workspace.context` replay for new TUI tasks,
  core-created tasks, resumed sessions, and child agents.
- Added Codex-style environment context as a user-role contextual fragment
  before the real task prompt, including cwd, shell, current date, and
  timezone.
- Ran a Codex-auth read-only reference check through a subagent. It judged the
  prompt layer materially aligned and AGENTS/context behavior partial.
- Ran the repository terminal UI verifier and inspected `/tmp/but-design-loop`
  artifacts for setup/ready, running, result, overlays, stopped-task, slash
  actions, and completed plain output.
- Added `AGENTS.override.md` precedence, global Codex-home instructions, source
  metadata, warning metadata, invalid UTF-8 replacement, Codex's 32 KiB project
  byte budget, global instruction trimming, byte-budget truncation, and
  non-blocking AGENTS read failures.
- Ran a second Codex-auth reference check for AGENTS behavior. It found the
  normal model-visible behavior matched and identified one UTF-8 warning-order
  edge gap, which was then fixed with a regression test. Config-layer and
  other edge-case semantics remain open.
- Ran a Codex-auth reference check for environment context. It found the simple
  local model-visible behavior matched, while network, subagent, multiple
  environment, and later-turn update semantics were still open at that point.
- Tightened `apply_patch` behavior so patch contents are parsed and planned
  before writes, hunk verification failures do not partially edit earlier
  files, and common Codex file-content semantics are matched.
- Audited Codex `apply_patch` safety through a reference subagent. It confirmed
  absolute and `..` paths are parsed first, then checked against writable roots
  and protected metadata; dirty-worktree protection is prompt-level rather
  than a runtime guard.
- Added the matching local no-approval safety slice: patch writes outside the
  task cwd, symlink escapes through existing parents, and top-level `.git`,
  `.agents`, or `.codex` metadata writes are rejected before any file write.
- Tightened `exec_command` behavior so simple read-only shell sequences can be
  parallelized in the same spirit as Codex's `is_known_safe_command`, while
  Codex-dangerous `rm -f`/`rm -rf` commands are rejected before spawning because
  this harness does not yet have Codex's approval UI or sandbox policy engine.
- Ran a Codex-auth reference check for shell policy. It confirmed that
  `git reset --hard`, `git clean -fd`, and `cargo test` are not
  dangerous-classifier hits in Codex; they remain part of the fuller
  approval/sandbox parity work.
- Added Codex-compatible `exec_command` approval metadata fields
  (`sandbox_permissions`, `justification`, and `prefix_rule`) and reject
  unsupported sandbox overrides before process spawn while this harness has no
  approval UI.
- Confirmed that approval-metadata behavior with a Codex-auth reference check:
  default sandbox metadata is accepted, non-default overrides reject with
  Codex's no-approval error string, and sandbox permission metadata is not
  emitted on public command-begin events.
- Matched Codex's hidden `additional_permissions` ordering for unified
  `exec_command`: the default schema does not advertise it, malformed payloads
  fail parsing first, and deserializable permission objects are treated as
  effective sandbox overrides that reject under `Never` with the same
  approval-policy message.
- Audited Codex's shell command safety classifier. Local already matched the
  main Unix safelist, then closed parser gaps where local accepted unquoted
  comments or trailing operators that Codex's tree-sitter parser rejects.
- Added Codex-derived command safety regressions for guarded `base64`, `find`,
  `rg`, `git`, shell wrappers, substitutions, redirection, empty pipeline
  segments, and the tiny Unix dangerous-command classifier. The local
  path-normalized `/bin/rm -rf` rejection remains an explicit no-approval
  hardening choice.
- Aligned `exec_command` and `write_stdin` model-visible descriptions with
  Codex unified-exec wording.
- Audited Codex unified-exec output shape. Direct model-visible
  `exec_command` and `write_stdin` results are plain text ordered as chunk id,
  wall time, exit/running state, original token count, and output; code-mode
  has a separate JSON shape.
- Changed local unified exec/write results to send that Codex-style text to the
  model, including token-style truncation markers, while keeping structured
  JSON in local `tool.finished` events so the TUI and state projection do not
  lose metadata.
- Audited and copied Codex's numeric session-id contract. Running command
  output now tells the model to use a positive numeric session id, the
  `write_stdin` schema advertises `session_id` as a number, and local
  `write_stdin` rejects string ids before registry lookup.
- Copied Codex's stdin model for unified exec. Non-TTY commands now see closed
  stdin by default, non-empty `write_stdin` input rejects with Codex's
  `stdin is closed for this session; rerun exec_command with tty=true to keep
  stdin open` message unless the session is TTY-backed, and empty polls still
  work.
- Audited Codex's unified-exec wait and output policies. The local runtime now
  keeps schemas permissive while enforcing Codex-style runtime defaults and
  clamps: `exec_command` waits stay in 250ms-30s, non-empty `write_stdin` waits
  stay in 250ms-30s, empty polls use the 5s-300s background range, and
  model-visible command output respects the current local tool-output token
  policy instead of unbounded model-requested limits.
- Audited and copied Codex's per-call retained output buffer. Local command
  output collection now uses a 1 MiB head/tail buffer, drains only newly
  collected output for each tool response, and preserves both prefix and suffix
  for very large outputs.
- Audited and copied Codex's model-facing `apply_patch` surface for Responses:
  the tool is now advertised as a `custom` freeform tool with Lark grammar,
  `custom_tool_call` items parse into raw patch-string tool calls, and replayed
  tool results return as `custom_tool_call_output` instead of JSON function
  output.
- Broadened local `apply_patch` raw patch compatibility for Codex-visible
  cases: raw string arguments, outer newline/CRLF tolerance, lenient heredoc
  wrappers, optional non-empty `*** Environment ID: ...`, whitespace-padded
  patch markers, first update chunks without an explicit `@@`, empty context
  lines, `*** End of File`, trailing-newline update semantics, and Codex-style
  `Success. Updated the following files:` output summaries.
- Tightened the remaining Codex patch semantics: `@@ text` is now preserved as
  a fuzzy single-line context anchor, EOF-marked update chunks must match final
  lines, pure-addition update chunks append at EOF, the freeform tool verifies
  all parsed hunks before runtime writes, verified patches apply sequentially
  at runtime, and success summaries include Codex's trailing newline.
- Verified the freeform `apply_patch` batch with focused core/provider tests
  and the full terminal verifier. The full verifier passed after deterministic
  setup/account/model/done/running/cancelled/browser/history/developer dumps
  and the real tmux smoke suite; artifacts were inspected under
  `/tmp/but-design-loop`.
- Audited Codex streaming custom-tool input handling. The reference behavior
  uses `response.custom_tool_call_input.delta` only for live patch-progress
  events; executable `apply_patch` input still comes from the final
  `response.output_item.done` custom tool call. Local now preserves that
  payload rule, emits progress-only `patch.updated` snapshots from streamed
  deltas, and dedupes repeated custom tool calls if a local mock/provider
  response also includes the same call in `response.completed.output`.
- Matched Codex's current `parallel_tool_calls` request default for bundled
  agent models. Local OpenAI/Codex Responses and OpenAI-compatible chat
  requests now derive the flag from model metadata instead of forcing `true`;
  the static bundled/unknown model slice currently sends `false`, as Codex's
  fallback and catalog tests do.
- Added Codex's no-approval permissions context with the same
  `<permissions instructions>` markers, danger-full-access sandbox sentence,
  and exact instruction not to provide `sandbox_permissions` under `Never`.
  OpenAI Responses input preserves it as a developer-role message.
- Re-injected canonical permissions plus AGENTS/environment context during
  compaction, matching Codex's pattern of carrying current initial context
  forward around summaries.
- Protected developer context in fallback providers: OpenAI-compatible chat
  maps it to system-priority messages, and Anthropic moves it into system
  blocks instead of treating it as an ordinary user message.
- Aligned `update_plan`'s model-visible success result with Codex's plain
  `Plan updated` text, and matched the main tool description/status schema
  wording so planning feedback does not drift across turns.
- Matched Codex's `update_plan` parser behavior for unknown fields and the
  documented-but-not-enforced multiple-`in_progress` case.
- Removed the local mandatory-explorer rule for repository/codebase analysis.
  Helper agents are now prompted like Codex: only after the user explicitly
  asks for sub-agents, delegation, or parallel agent work.
- Aligned the local `spawn_agent` surface with Codex multi-agent v2 for the
  model-visible contract: required `task_name` plus `message`, string
  `fork_turns`, no legacy `fork_context` in the advertised schema, full-history
  override rejection, partial-fork model/reasoning override propagation to
  child runners, and `task_name`/`nickname` output instead of child result
  payloads.
- Added Codex-style AGENTS project-discovery config for
  `project_doc_fallback_filenames` and `project_root_markers`. A Codex-auth
  reference check confirmed configured "user instructions" are the existing
  global AGENTS path, while these two config keys control project-doc fallback
  names and root traversal.
- Added Codex-style `project_doc_max_bytes` handling for AGENTS project docs,
  including `0` disabling project docs while preserving global instructions.
- Matched Codex's project AGENTS error behavior: directories and special files
  are ignored, invalid UTF-8 remains lossy with a warning, truncation is not a
  startup warning, and non-`NotFound` project metadata/read failures omit the
  project-doc block instead of leaking warnings into model-visible context.
- Added trusted project-local AGENTS config layering: `.browser-use/config.toml`
  may override `project_doc_max_bytes` and `project_doc_fallback_filenames`
  after the project is trusted in the user config. Project-local
  `project_root_markers` is ignored for AGENTS discovery, matching Codex's
  non-circular root-selection rule.
- Added Codex's file-backed AGENTS config precedence for the implemented
  layers: Unix system config, Browser Use Terminal home user config, trusted
  project config, then managed `managed_config.toml`. Managed config can override
  `project_root_markers` for AGENTS discovery because Codex merges non-project
  layers for that decision after the final stack exists.
- Added Codex profile-v2 AGENTS config layering for selected profiles:
  `$BROWSER_USE_TERMINAL_HOME/<name>.config.toml` is treated as a second user
  layer above base user config, can override project-doc settings and
  `project_root_markers`, and can participate in project trust. The local runner
  also validates plain profile names and rejects selected legacy
  `profile`/`[profiles.<name>]` conflicts like Codex.
- Added Codex `-c/--config key=value` session override behavior for AGENTS
  config: direct CLI/provider runs parse TOML values with raw-string fallback,
  build dotted-key override tables, use those overrides for pre-project
  root/trust discovery, apply them above trusted project config, and keep
  managed config higher precedence.
- Added managed-config MDM behavior for AGENTS config: Unix managed config
  defaults to `/etc/browser-use-terminal/managed_config.toml` outside tests, and
  macOS `com.browseruse.terminal:config_toml_base64` managed preferences are
  decoded and applied as the highest-precedence AGENTS config layer.
- Matched Codex's AGENTS-impacting config-load fatality rules: missing config
  files/preferences are no-ops, malformed or unreadable system/user/profile and
  managed layers fail startup, trusted project config parse errors fail startup,
  untrusted project config parse errors are ignored, and trusted
  project-local ignored keys remain startup warnings.
- Confirmed Codex's initial context aggregation shape: base/model instructions
  stay in the provider instruction channel, developer fragments are aggregated
  as developer content, and AGENTS/environment context is grouped as contextual
  user content before the real user task.
- Confirmed Codex's turn-context baseline and reconstruction behavior:
  `TurnContextItem` is durable metadata, compaction replacement history is a
  replay checkpoint, rollback trims user-turn segments plus adjacent contextual
  updates, and app-server resume/fork/rollback surfaces are product wiring
  around those core heuristics.
- Added local `context.baseline` events after each real user turn's context and
  model settings are resolved. These events are not sent to the model, but they
  give future resume/fork/rollback work a durable Codex-shaped baseline instead
  of inferring everything from raw `model.config` and `workspace.context`
  events.
- Stored compacted `replacement_messages` on `session.compacted` and changed
  provider replay to start from the newest checkpoint, while still preserving
  first post-compaction `before_seq` context updates before their target
  follow-up.
- Added rollback-marker replay filtering. Local `session.rollback` events are
  not sent to the model; they cause provider replay to drop the newest N real
  local user turns, trim `workspace.context`, `<model_switch>`, and
  `<personality_spec>` events anchored to those turns, preserve the pre-first
  prompt context prefix, and ignore rolled-back `model.config` events when
  deciding later model-switch context.
- Added baseline-backed previous settings reconstruction. Model-switch and
  personality diff decisions now read the latest surviving `context.baseline`
  when available, ignore baselines not tied to a real user turn, hydrate from
  compacted checkpoint metadata, and only fall back to legacy `model.config`
  events for sessions created before baseline events existed.
- Added model-switch reasoning-effort normalization. The local provider
  metadata now includes bundled GPT/Codex supported effort lists, and resumed
  model switches clamp unsupported efforts such as `minimal` to the target
  model's middle supported effort while leaving reasoning summary and verbosity
  untouched.
- Added Codex-style `developer_instructions` config handling for the local
  developer context. The same system/user/profile/project/session/managed
  precedence stack applies, and managed config can override session overrides.
- Confirmed Codex's base-instruction config behavior: `instructions` is an
  inline base override, `model_instructions_file` is a file-backed base
  override that beats inline instructions, and `developer_instructions` stays
  separate in developer context.
- Added that base-instruction config behavior locally. File paths resolve
  relative to the config layer, empty files fail like Codex, project-local
  `.codex` files are supported when trusted, and resolved text is passed as
  provider `instructions` instead of a user/developer message.
- Confirmed Codex's broader environment-context renderer: zero/single/multiple
  environments, optional network allow/deny requirements, optional active
  subagent lines, and `include_environment_context` disabling all belong in the
  user-role contextual fragment.
- Added that renderer surface locally. Active local descendants now refresh the
  environment context as `<subagents>`, and the same config stack can disable
  environment context with `include_environment_context = false`.
- Added Codex-style AGENTS startup-warning and instruction-source surfacing:
  invalid UTF-8 and AGENTS/config warning text is emitted as session warning
  events without becoming model-visible prompt text, loaded AGENTS source paths
  are recorded as first-class session metadata, and the TUI surfaces warnings
  in the transcript plus sources/warnings in developer diagnostics.
- Confirmed Codex's model request settings path: reasoning effort, reasoning
  summary, and text verbosity are config/profile/session values that shape the
  Responses request, while full defaulting and capability gating comes from
  Codex `ModelInfo`.
- Added the local config-to-request slice. The same Codex-style config stack
  now sets `ProviderTurn` model request settings, OpenAI Responses and Codex
  Responses serialize `reasoning.effort`, omit `reasoning.summary` for
  `none`, include encrypted reasoning content when reasoning is present, and
  serialize `text.verbosity`. Codex Responses keeps the existing default low
  verbosity.
- Added Codex-style `ModelInfo` request gating for the bundled agent models.
  Known models now get Codex defaults for reasoning effort, reasoning summary,
  and verbosity; unknown models omit unsupported reasoning and verbosity; a
  true `model_supports_reasoning_summaries` force-enables reasoning summaries;
  and lookup follows Codex's longest-prefix plus one-level namespaced-suffix
  behavior. The bundled GPT/Codex model slice now also matches Codex's current
  `supports_parallel_tool_calls=true` catalog values for `gpt-5.5`,
  `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex`, `gpt-5.2`, and
  `codex-auto-review`.
- Added the first model-personality/base-instruction slice. The local provider
  now starts default instructions with a Codex-shaped model instruction header,
  supports config `personality = "none" | "friendly" | "pragmatic"`, defaults to
  pragmatic, and preserves the rule that explicit `instructions` or
  `model_instructions_file` replaces the model prompt instead of being wrapped
  by personality text.
- Added session-persisted base instructions. Codex resolves base instructions
  as explicit config/API override, then resumed/forked session metadata, then
  current model/personality defaults. Local now records the resolved value as a
  `session.base_instructions` event, reuses it across later provider runs when
  only defaults/personality changed, lets explicit `instructions` override it
  for the current run, and copies the parent run's resolved value into spawned
  child sessions.
- Added Codex-style model-switch context. When the current provider model
  differs from the previous `model.config`, local now keeps the persisted base
  instructions in the request `instructions` field and adds a developer
  `<model_switch>` message before the resumed user message. The event is
  persisted with a `before_seq` anchor so replay preserves that ordering and
  later same-model runs do not create duplicates.
- Added Codex-style personality-only context. The runtime records personality
  on `model.config`, uses the same-model/no-model-switch path to emit a
  developer `<personality_spec>` message when the frozen base prompt does not
  bake the active personality, and anchors the event before the current user
  message so replay stays model-visible in the same place.
- Added Codex-style later-turn ordering for refreshed environment context.
  Environment `workspace.context` updates that are generated after a follow-up
  now carry a `before_seq` anchor, and provider replay moves that contextual
  user message before the real follow-up prompt, matching Codex's
  developer-diff, contextual-user-diff, user-prompt order.
- Tightened contextual replay classification after the Codex reference audit
  for `contextual_user_message`, `event_mapping`, and history rollback. Local
  replay now treats first-user-anchored environment context as part of the
  initial contextual user bundle, but keeps later anchored environment updates
  immediately before their target follow-up. Compaction still reinjects the
  latest canonical context snapshot.
- Added Codex-style shell-insensitive environment update equality. Local
  environment context events now store a structured snapshot and suppress
  another model-visible `workspace.context` refresh when only the shell changed
  or, for a single environment, only the environment id changed. Cwd, date,
  timezone, network, and multiple-environment id/cwd changes still trigger
  refreshes.
- Added Codex-style minimized environment update bodies for later turns. Cwd
  changes emit `<cwd>` and `<shell>`, network-only changes emit date/timezone
  plus the network block without cwd/shell, multiple-environment changes emit
  the full `<environments>` block, and subagent-only changes do not create
  later environment diff updates.
- Added Codex-style `agent_type` role validation and the first role-layer
  application slice. The local `spawn_agent` schema now advertises the current
  Codex built-ins (`default`, `explorer`, and `worker`), unknown non-full-fork
  roles are rejected before a child is created, and configured role files can
  override child model/reasoning plus flow `developer_instructions` and
  `instructions` into the child turn after requested spawn overrides.
- Added Codex-style `spawn_agent.service_tier` handling for the modeled
  bundled models. The v2 schema exposes the optional override, full-history
  forks can still use it, unsupported requested tiers fail with Codex-shaped
  model-specific errors, and role/requested/parent tiers are filtered against
  the effective child model before the child run is launched.
- Added Codex-style discovered user-defined agent roles. File-backed config
  layers now scan recursive `agents/*.toml` role files, malformed discovered
  roles become startup warnings, discovered roles participate in runtime
  `agent_type` validation/config application, and the model-visible
  `spawn_agent` schema lists user roles before built-ins.
- Added Codex-style role metadata and nickname allocation. Declared and
  discovered roles now validate `nickname_candidates`, role-file metadata can
  override inline declared metadata including role name, malformed declared
  roles become startup warnings, and `spawn_agent` stores/returns a generated
  nickname from the role candidates or Codex's default nickname pool.
- Added Codex-style locked-setting notes to `spawn_agent.agent_type` schema
  text. When a role config file locks `model`, `model_reasoning_effort`, or
  `service_tier`, the model-visible role description now explains that with
  Codex's exact wording; unreadable or malformed role config files simply omit
  the extra note during schema rendering.
- Added Codex-style hidden spawn metadata behavior. The nested
  `[features.multi_agent_v2] hide_spawn_agent_metadata = true` flag now hides
  `agent_type`, `model`, `reasoning_effort`, and `service_tier` from the
  `spawn_agent` schema, returns only `task_name` to the model, and still keeps
  the generated child nickname in local session metadata/events like Codex.
- Added Codex-style model override catalog guidance to `spawn_agent`. The tool
  description now lists the bundled picker-visible model slice with model
  descriptions, supported reasoning efforts, default effort markers, and
  service tier ids, while `hide_spawn_agent_metadata` suppresses the catalog.
  The local bundled metadata was corrected so `gpt-5.4-mini` no longer
  advertises or serializes the unsupported `priority` service tier.
- Audited Codex MultiAgentV2 management tools. The v2 reference surface has no
  legacy `send_input` or `resume_agent`; it uses plain-text `send_message` and
  `followup_task`, a targetless `wait_agent`, root-tree `list_agents`, and
  `close_agent` with `previous_status`.
- Aligned the local v2 management tools to that surface. `wait_agent` now
  returns only `{message,timed_out}` without child content, `list_agents`
  returns compact root-tree rows, `close_agent` rejects root and returns the
  prior status, and message/follow-up tools reject legacy fields while keeping
  Codex root-target behavior.
- Added the represented MultiAgentV2 runtime heuristics from the later audit:
  configured concurrency/wait/usage-hint keys, spawn concurrency enforcement,
  mailbox-only wait wakeups, one-shot mailbox drain into target model turns,
  child-to-parent `<subagent_notification>` completion mail, and root/subagent
  standalone developer usage hints with parent-hint filtering during forks.

## Closure Audit Result

On 2026-05-24, ten read-only Codex-auth subagents audited the current branch
against `/home/exedev/repos/codex`. The result was uniformly open: the visible
surface is much closer than at the start of the branch, but several classes of
Codex agent-quality behavior are still missing or adapted.

The biggest remaining categories are:

- Provider transport and stream handling: the main Responses request shape,
  normal streaming path, failed/incomplete response handling, reasoning usage,
  EOF-before-completed error behavior, output-schema text format, typed error
  classes, hidden request retries, selected-provider retry config, response
  header metadata events, event-level server-model metadata,
  model-verification events, SSE idle timeouts, provider stream retry config,
  HTTP `client_metadata.x-codex-installation-id`, sticky `x-codex-turn-state`
  request metadata, and rate-limit token coupling are now implemented. Still
  open: websocket client metadata, W3C trace metadata, websocket or alternate
  transport fallback including `response.processed`, auth-refresh retry, and
  app-server notification surfaces.
- Tool surfaces: the audited direct v2 terminal tool-schema surface is now
  aligned. Tool specs retain Codex output schemas internally while omitting them
  from Responses tool JSON, `view_image` uses Codex detail/default/output
  behavior for bundled model metadata, local-only file helpers are no longer
  model-visible, and legacy hidden spawn fields reject like Codex v2.
- Unified exec internals: env defaults, default login behavior, explicit-shell
  fallback, process id allocation/pruning, background completion, per-call
  output buffering, final transcript events, no-newline partial output,
  stdin write-error model text, cross-turn background process lifetime, and the
  represented Unix tree-sitter shell parser path are now partially aligned.
  Shell type/path resolution, shell-specific argv derivation, six-hex chunk ids,
  and the non-empty stdin reaction window are now aligned for the represented
  runtime. Still open: optional zsh-fork behavior, deeper non-Unix
  policy/parser semantics beyond argv derivation, pause-aware waits,
  notification/post-exit drain exactness, here-doc metadata, code-mode JSON, and
  app-server/alternate-product shutdown wiring if those entry points are added.
- Patch semantics: contextual/parser semantics, preverification/runtime
  sequencing, streaming custom-tool progress snapshots, success/failure
  lifecycle events, and committed-delta reporting after runtime failures are
  now partially aligned. Still open: exact app-server patch notification
  surfaces, fuller streaming parser/throttle behavior, and sandbox-dependent
  hardlink/path protections.
- Collaboration and Plan mode: exact Default/Plan collaboration templates,
  config-gated injection, later-turn developer updates, Plan-mode `update_plan`
  rejection, local TUI/CLI mode controls, the `request_user_input` answer path,
  structured option/notes/unanswered handling, active and completed question
  rendering, Plan-mode-only proposed-plan rendering, active-turn `/plan` safety,
  the Plan-mode medium reasoning preset, the default-mode request-input feature
  flag, and `turn_id`-preferred request-input responses are now implemented.
  Remaining open work is app-server-compatible collaboration/request lifecycle
  events and settings broadcasting, pending interactive replay/filtering,
  delegated/MCP compatibility paths, and broader feature-flag coverage.
- Turn lifecycle and finalization: interrupted-turn replay now includes Codex's
  model-visible `<turn_aborted>` marker and `aborted` missing-call outputs, and
  local sidecars now cover Codex-shaped task start/complete/abort,
  final-answer extraction, retry stream errors, consumed token totals,
  recomputed active-context token counts, and latest rate-limit snapshots.
  Still open: fully typed app-server lifecycle surfaces,
  config-gated/developer-role interrupt marker variants, local-only compaction
  timing/baseline nuances, exact typed token estimation, and resume/fork context
  breadth.
- Full config/model and multi-agent systems: the represented model catalog
  substrate now drives request metadata, tool capability gates, spawn-agent
  validation, tool-output truncation, remote/cache refresh, TUI picker rows,
  and TUI `--profile`/`-c` runtime propagation. AWS/Bedrock/Mantle support is
  intentionally removed as provider baggage. Still open: complete
  `ConfigLayerStack`, thread snapshots, v1 multi-agent tools, true app-server
  mailbox/input-queue semantics, and app-server bridge events.

## Still Open

- Exact Codex instruction layering beyond the now-aligned no-approval
  permissions fragment, simple AGENTS plus environment startup context,
  config-backed `developer_instructions`, config-backed base instructions,
  typed runtime base/developer instruction overrides, session-persisted base
  instructions, compaction reinjection, and fallback-provider priority mapping.
  Still open: separate developer
  exceptions, apps/skills/plugins/collaboration/personality/realtime developer
  sections, precise later-turn permission/profile updates, token-accounting use
  of frozen session base instructions, and rollout/resume reference-context
  semantics.
- Provider request and streaming parity beyond the implemented G-026 batch:
  HTTP identity headers, `client_metadata.x-codex-installation-id`, sticky
  `x-codex-turn-state`, represented `x-codex-turn-metadata`,
  `x-codex-beta-features`, HTTP full-history fallback behavior, custom provider
  request shaping, command-backed provider auth, Claude OAuth 401 recovery,
  managed Codex auth 401 recovery, represented remote/cache model metadata, and
  provider-object default-resume snapshots are now implemented. Still open:
  websocket client metadata and W3C trace metadata, attestation headers, richer
  app-server/turn-metadata enrichment, websocket or alternate transport
  fallback, client-side websocket `response.processed` ack, app-server thread-settings/session-config
  notification surfaces beyond the core
  `session.config_snapshot`, and app-server lifecycle/token replay surfaces.
- Tool schema parity beyond the now-closed direct v2 terminal schema surface:
  v1 namespaced multi-agent tools, true app-server mailbox behavior, and
  non-schema runtime semantics tracked in the exec/patch/multi-agent rows.
- Full runtime shell approval, sandbox, and execpolicy behavior: rule files,
  prefix-rule approvals, feature-enabled additional-permission normalization
  and approval, network denial handling, full tree-sitter parser parity beyond
  the tested Unix plain-command edge cases, deeper non-Unix shell policy/parser
  semantics beyond the now-aligned PowerShell/Cmd argv derivation, optional
  zsh-fork behavior, pause-aware wait
  deadline extension, notification/post-exit drain exactness, here-doc metadata,
  code-mode JSON if this terminal adds code-mode, app-server/alternate-product
  shutdown wiring if those entry points are added, and exact prompt-vs-allow behavior for non-dangerous unmatched
  commands.
- Full patch sandbox execution, protected-path approval prompts, and approval
  decision logic are intentionally out of scope for this experiment unless the
  product later needs them. Still open for patch quality: hardlink-escape
  protection that depends on sandboxing, exact app-server patch notification
  surfaces, and fuller streaming parser/throttle behavior. Dirty-worktree
  ownership remains prompt-level in Codex rather than a runtime patch guard.
- A repeatable Codex-auth golden-task harness beyond the current subagent
  reference checks.
- Exact Codex app-server `configWarning` and thread-response
  `instruction_sources` protocol shapes are product-wiring differences unless
  this terminal grows an app-server-compatible API surface.
- Complete Codex `TurnContextItem` reconstruction beyond the local
  `context.baseline` events: exact thread/environment manager ordering for
  multiple environments, realtime previous-setting parity,
  fork/interrupted-turn metadata, legacy/lossy compaction baseline clearing,
  and rollout/app-server reference-context persistence.
- Full contextual history semantics: public rollback command/API and active-turn
  validation, exact rollback token-event ordering/app-server token replay,
  interrupted fork snapshots, mixed developer-bundle baseline invalidation, and
  marker classification for hook, skill, goal, user-shell, and warning fragments
  that this harness does not yet model.
- Codex model-catalog/personality/model-switch behavior:
  the bundled GPT/Codex catalog and fallback prompt are now vendored from Codex
  byte-for-byte, and catalog `ModelMessages` now follow Codex's
  personality-only replacement semantics before the browser overlay is appended.
  Model-switch instructions and personality-update gating use the same active
  catalog substrate.
  Request metadata, hidden/non-picker entries, fallback personality slugs,
  spawn-agent model guidance/validation, configured `model_catalog_json`, a
  fresh offline `models_cache.json` path, Codex-auth root `/models`, command-auth
  provider `/models`, ETag refresh/renewal, tool-output truncation policy, and
  TUI picker rows, config `model`, normal CLI Codex/OpenAI defaults, TUI
  startup defaults, TUI provider-id routing, TUI `--profile`/`-c` propagation
  into model/provider resolution, workspace context, and runtime options,
  Codex/OpenAI dataset runs, and CLI config defaults now share one active
  config/catalog substrate. Core
  `session.config_snapshot` now also carries a Codex-shaped provider object for
  default resumes. AWS/Bedrock/Mantle support is intentionally removed as
  provider baggage. Still open: full `ConfigLayerStack`,
  thread/project layer behavior,
  full app-server provider/model snapshot protocol surfaces on resume/fork
  beyond the represented core event, richer Codex doctor/status-style config
  displays, unsupported-verbosity warning
  telemetry, and deeper app/MCP/code-mode output surfaces not represented by
  this terminal runtime.
- Plan-mode collaboration behavior beyond the implemented collaboration
  instructions, mode controls, request-input answer UI, proposed-plan rendering,
  reasoning preset, default-mode request-input gate, and turn-id answer path:
  full app-server collaboration/request events, pending interactive
  replay/filtering, delegated/MCP compatibility paths, and broader feature-flag
  breadth.
- Runtime tool parity now includes the portable apply-patch rescue path Codex
  uses when a model sends `apply_patch <<EOF ... EOF` through `exec_command` or
  the legacy shell command surface: the harness intercepts the script and routes
  it through the apply-patch implementation without spawning a shell process.
  Patch completion and `turn.diff` events now include bounded unified diffs
  generated from committed patch deltas. Hook payload names also follow Codex's
  canonical names for `apply_patch` and `spawn_agent`, while matcher aliases
  cover `Write`/`Edit` and `Agent`. Bash/apply_patch hook inputs now expose
  `tool_input.command`, and hook `updatedInput.command` rewrites back into the
  local tool argument shape. The rescue path also covers `applypatch`, direct
  single-argument invocation, and strict `cd <path> && apply_patch <<EOF`
  forms. Invalid-image recovery, compaction overflow retry, execution of
  `async_run` hook effects, concurrent command-hook execution, completion-order
  `updatedInput`, and inexact dirty-baseline git diff reporting are now
  represented. Hook run summaries, source/trust metadata, transcript snapshots,
  and `SubagentStop` child/parent transcript paths are represented for command
  hooks. Still open: prompt/agent hook handler variants, exact trust
  enforcement/listing UX, plugin/cloud hook discovery, stricter output
  validation, exact streaming futures for mutating tools, and a full Codex
  `TurnDiffTracker` lifecycle across every tool runtime.
- Multi-agent family routing now follows Codex's feature gate for represented
  sessions: `features.multi_agent_v2.enabled = true` selects the v2 task-path
  surface, while the default surface is the namespaced legacy
  `multi_agent_v1` family. The v1 wrappers cover `spawn_agent`, `send_input`,
  `resume_agent`, `wait_agent`, and `close_agent` with Codex-shaped schemas and
  output payloads, adapting to the local session store. The latest slice also
  preserves typed v1 `items` for child context: Codex-style `message`/`items`
  validation is enforced, text/image/local_image/skill/mention inputs persist
  alongside preview text on `session.input` and `session.followup`, provider
  replay uses typed content parts, `skill` items inject Codex-shaped `<skill>`
  context from `SKILL.md`, and v1 `send_input` bypasses the string-only mailbox
  for model-visible input. Local now also parses model `supports_search_tool`,
  exposes Codex-shaped `tool_search`, parses `tool_search_call`, and defers
  default `multi_agent_v1` tools behind BM25 discovery when the selected model
  and provider support search plus namespace tools. V1 spawn/resume/close now
  use id-only references, direct user-role completion notifications, missing-id
  resume errors, and target-edge-only close semantics for the represented local
  tree.
- Full multi-agent parity: mention/plugin/app context injection, CLI legacy
  spawn/resume/wait commands, arbitrary role config keys such as
  sandbox/skills/tools, exact non-file config layer semantics, exact app-server
  collaboration/input-queue event bridge semantics, full rollout persistence
  for resume/reopen beyond this local store adaptation, code-mode-only runtime
  separation if local adds code mode, and deeper typed rollout persistence
  beyond the local event-store adaptation.
- Codex turn lifecycle parity: fully typed `TurnComplete`/`TurnAborted` events,
  websocket-only previous-response transport placement, `response.processed`
  ack and sticky fallback, TTFT/duration/app-server notification exactness,
  config-gated/developer-role interrupt marker variants, local-only compaction
  timing/baseline nuances, exact resume/fork `TurnContextItem` persistence, full
  typed auth-refresh transport retry, exact typed token estimation, and full
  app-server compaction/token-window lifecycle.
- The latest runtime slice adds a per-turn `ToolRouter` substrate. Tool
  planning, model-visible specs, deferred `tool_search` indexing, search-source
  descriptions, parallel/read-only streaming eligibility, serial dispatch, and
  parallel dispatch now share one router object for the turn instead of
  rebuilding independent registries. This closes the represented static-router
  inconsistency and moves the local architecture closer to Codex's
  `ToolRouter`; the router also caches its BM25 deferred-tool search engine for
  the turn rather than rebuilding it per query. The same pass fixed explicit
  goal `updated_at_ms` preservation for imported/created goal events. What
  remains is real dynamic callable contribution from MCP/app/plugin/extension
  sources plus Codex's async tool-future lifecycle.
- The current streaming-runtime slice uses that router to start every
  parallel-safe direct tool during provider streaming, not only read-only file
  tools. Read-only calls still carry the old `read_only_predispatch` event
  label; visible terminal tools such as `exec_command` now carry
  `parallel_predispatch` and can run before the provider stream completes when
  hooks and queued user input do not block them. This closes the represented
  latency gap for Codex-parallel-safe local tools while keeping model-visible
  tool outputs ordered by the model's call order. It does not pretend to close
  the deeper async runtime: cancellation, read/write gates for non-parallel
  calls, and dynamic contributed tool executors still need larger substrates.
- Ten real Codex-auth child-agent audits after the router slice found no new
  easy client-side correctness blocker. Their consensus was that the remaining
  agent-quality gaps are architectural: dynamic tool contributors, cancellable
  async tool futures with read/write gating, first-class active-turn state,
  typed history/rollout/context ownership, exact turn diffs, goal runtime
  accounting, local compaction/token-window fidelity, and deeper skills/plugins
  and review-task lifecycle.
- Verification for the streaming-runtime slice passed after isolating
  `CODEX_HOME` in the new default-unified-exec regression test. The full local
  gate passed (`cargo fmt --check`, `git diff --check`, Python tests, and full
  `cargo test`), and the live Codex-auth smoke used root session
  `84c40f19c627` plus child session `9c649acf6e21`; the root returned `Paris`,
  and the child read `/tmp/but-codex-agent-parity-smoke.txt` as
  `agent-parity-smoke-ok`.
- A fresh ten-agent broad audit after this slice found no new correctness
  regression. The auditors agreed the new `parallel_predispatch` path is a
  useful latency improvement with an expected ordering risk surface, and that
  the strict active-input and hook gates are the right guardrails for now. The
  remaining provider-neutral gaps are still architectural: dynamic tool
  contributors, cancellable async tool futures with read/write gates,
  first-class active-turn state, typed history/replay ownership, exact turn
  diffs, local compaction/token precision, subagent lifecycle depth, and
  skills/plugins/review runtime depth.
- The next streaming-runtime guardrail closes the concrete ordering risk
  surfaced by that audit. During a streamed provider attempt, the scheduler now
  installs a predispatch barrier as soon as it sees a serial tool, queued
  active-turn input, or matching runtime hooks. Later parallel-safe calls in the
  same streamed attempt then wait for normal ordered dispatch instead of
  overtaking the earlier call. This matches the observable behavior Codex gets
  from its read/write-gated async tool runtime for the represented local tools,
  while still leaving the full cancellable future lifecycle and dynamic
  contributed executors as larger open work.
- Verification for the guardrail passed the full local gate: formatting,
  whitespace, Python tests, and full Rust workspace tests. The live Codex-auth
  smoke used root session `a9c3a83248da` and child session `7bf7aaa46710`; the
  root returned `Paris`, and the child read
  `/tmp/but-codex-agent-parity-smoke.txt` as `agent-parity-smoke-ok`.
- A second ten-agent broad audit after the guardrail found no new correctness
  regression. The auditors' consensus was that the barrier closes one concrete
  streamed-tool ordering hole, while the remaining gaps are still the large
  provider-neutral runtime pillars: dynamic tool contributors, cancellable
  async tool futures with read/write gates, first-class active-turn/thread
  state, typed history/replay ownership, deeper local compaction/token
  lifecycle, richer subagent lifecycle ownership, and deeper hooks,
  skills/plugins, and review integration. Some individual reports used stale or
  shallow wording about hook/skill absence, so those claims were not treated as
  stronger evidence than the direct source/tests already in this log.
- The current turn-diff slice tightens another concrete runtime-history edge:
  git-backed `turn.diff` snapshots now include staged index changes as well as
  unstaged worktree changes. Before this, a command that wrote a file and ran
  `git add` could produce an exact-looking turn-diff event with file names but
  an empty `unified_diff`; Codex's turn diff tracker is not blind to the git
  index in that way. Local still does not claim the full shared in-memory
  `TurnDiffTracker` architecture, but clean-baseline staged edits now produce
  model-visible patch text instead of an empty diff.
- Verification for the staged turn-diff slice passed the full local gate:
  formatting, whitespace, Python tests, and full Rust workspace tests. The live
  Codex-auth smoke used root session `8965e085a54f` and child session
  `cdedc06e4946`; the root returned `Paris`, and the child read
  `/tmp/but-codex-agent-parity-smoke.txt` as `agent-parity-smoke-ok`.
- A fresh ten-agent broad audit after the staged turn-diff slice found no new
  branch-local regression. All ten auditors treated the staged-index diff
  change as a useful fidelity fix. Their consensus was that the remaining gaps
  are still architectural: dynamic tool contributors, cancellable async tool
  futures with read/write gates, first-class active-turn state, typed
  history/replay ownership, deeper local compaction/token lifecycle, richer
  subagent lifecycle ownership, hooks/skills/plugins/review integration,
  provider/model policy layering, extension/memory depth, and the full
  in-memory `TurnDiffTracker`.
- The current goal-runtime slice closes several client-side accounting gaps
  from Codex's goal extension. Goal usage now consumes uncached input plus
  output tokens rather than raw total tokens; this prevents cached input and
  reasoning-only deltas from falsely exhausting a goal budget. Goal tool
  responses now include Codex-shaped `goal`, `remainingTokens`, and
  `completionBudgetReport` fields while preserving the local response shape.
  Active goals that cross their token budget are marked `budget_limited`, the
  next tool finish injects a one-shot hidden wrap-up `<goal_context>`, and
  non-retryable provider usage-limit failures mark active or budget-limited
  goals `usage_limited`. This is still event-store based rather than Codex's
  full state-db extension runtime, so exact wall-clock accounting, external
  app/server mutations, and first-class active-turn `InputQueue` integration
  remain open.
- Verification for the goal-runtime slice passed the full local gate:
  formatting, whitespace, Python tests, and full Rust workspace tests. The live
  Codex-auth smoke used root session `a79c499dbbc5` and child session
  `46c84d2bd649`; the root returned `Paris`, and the child read
  `/tmp/but-codex-agent-parity-smoke.txt` as `agent-parity-smoke-ok`.
- A fresh ten-agent broad audit after the goal-runtime slice found no
  non-goal regression. The auditors treated effective token accounting,
  `completionBudgetReport`, `budget_limited` wrap-up steering, and
  `usage_limited` provider-error handling as useful parity movement. Their
  repeated caution is that this branch-new budget-limited state must stay
  covered because it now steers model behavior. Remaining goal work is the
  larger lifecycle layer: persistent goal state, exact wall-clock accounting,
  external mutation callbacks, TUI/protocol rendering of budget-limited state,
  and active-turn `InputQueue`/`AgentControl` integration. The broader
  non-goal gaps remain typed history/replay, dynamic skill/plugin/tool
  contributors, hook lifecycle depth, subagent lifecycle depth, and
  provider/model policy layering.
- The current goal-accounting slice closes the next concrete Codex goal-runtime
  gap without adopting remote/server-only goal machinery. Goal time is no
  longer derived from `now - created_at_ms`; it is accumulated from explicit
  `goal.accounted` events emitted during active runtime checkpoints. The same
  accounting events freeze stopped-goal usage so completed goals no longer drift
  when later unrelated `token_count` events arrive. Runtime checkpoints now use
  the active turn start as the wall-clock baseline and Codex-style effective
  token deltas as the token baseline. `create_goal` also returns Codex's
  positive-budget validation wording.
- Verification for the goal-accounting slice passed the full local gate after
  the audit-surfaced duplicate-charging regression was added: formatting,
  whitespace, Python tests, and full Rust workspace tests. The full workspace
  run passed with browser-use-browser 16 passed plus 2 ignored browser smokes,
  CLI 18, core 435, protocol 19, providers 104, python-worker 11, store 15,
  TUI 140, and doc-tests. The live Codex-auth smoke used root session
  `bfa5626ce30f` and child session `4c5d285fcf07`; the root returned `Paris`,
  and the child read `/tmp/but-codex-agent-parity-smoke.txt` as
  `agent-parity-smoke-ok`.
- Ten broad real child-agent audits after this slice agreed that the new
  branch-local behavior was goal accounting, not a broad prompt/provider/tool
  regression. Two auditors independently called out possible duplicate charging
  across the new model-usage, assistant-turn, and tool-output checkpoints; the
  new `goal_accounting_checkpoints_do_not_double_charge_tokens` regression
  covers that path. Several reports over-counted out-of-scope SDK/server
  surfaces or repeated stale "hooks/skills absent" language, so those claims
  were filtered against the already implemented and tested hook, skill, plugin,
  and review slices. The remaining material gaps are still architectural:
  dynamic callable contributors, cancellable async tool futures with read/write
  gates, first-class active-turn/thread control state, typed
  history/rollout/replay ownership, deeper local compaction/token lifecycle
  precision, persistent goal state and external mutation callbacks, richer
  subagent lifecycle ownership, and review/plugin/skill manager depth.
- The current skills slice closes a model-visible prompt hygiene gap in the
  local skill inventory. Instead of dumping every skill description until an
  arbitrary count cap, available skills now render through a Codex-shaped
  metadata budget: 2% of the configured model context window when known, or an
  8k character fallback. The renderer fairly shortens descriptions before
  omitting skills, keeps higher-priority scopes first when minimum lines exceed
  the budget, emits provider-neutral startup warnings when metadata is
  compressed, and uses `### Skill roots` aliases when shared path roots let more
  skills fit. This matters because skill inventory lives in the model's
  developer context on every turn; unbounded descriptions can waste context, and
  silent omission can make the model miss reusable local capabilities.
- Verification for the skills slice passed the full local gate: formatting,
  whitespace, Python tests, and full Rust workspace tests. The full workspace
  run passed with browser-use-browser 16 passed plus 2 ignored browser smokes,
  CLI 18, core 439, protocol 19, providers 104, python-worker 11, store 15,
  TUI 140, and doc-tests. The live Codex-auth smoke used root session
  `4f462619aa02` and child session `18051821d0a3`; the root returned `Paris`,
  and the child read `/tmp/but-codex-agent-parity-smoke.txt` as
  `agent-parity-smoke-ok`.
- Ten broad real child-agent audits after this slice treated the skill metadata
  rewrite as the only branch-local model-visible change. The consensus was that
  it is a useful parity improvement rather than a regression, with the normal
  caution that budget and warning behavior now affects what the model sees.
  Several individual reports repeated stale "hooks/skills/plugins/goals absent"
  language; those claims were filtered against implemented code and focused
  tests. The remaining material gaps are unchanged: dynamic callable
  contributors from MCP/apps/plugins/extensions, cancellable async tool futures
  with read/write gates, first-class active-turn/thread control state, typed
  history/rollout/replay ownership, deeper local compaction/token lifecycle
  precision, persistent goal state and external mutation callbacks, richer
  subagent lifecycle ownership, richer review flow, and skill/plugin manager
  depth beyond prompt rendering.
- The current plugin-hook slice closes a concrete extension/runtime gap.
  Enabled local Codex plugin bundles can now contribute command hooks through
  the same hook runtime used by workspace/user config. The loader supports the
  default plugin `hooks/hooks.json`, manifest-provided hook file paths, arrays
  of hook files, and inline manifest hook objects, while preserving plugin
  source metadata for lifecycle events. This is deliberately local and
  provider-neutral; it does not add Codex cloud/plugin marketplace behavior.
- Verification for the plugin-hook slice passed the full local gate:
  formatting, whitespace, Python tests, and full Rust workspace tests. The
  full workspace run passed with browser-use-browser 16 passed plus 2 ignored
  browser smokes, CLI 18, core 442, protocol 19, providers 104,
  python-worker 11, store 15, TUI 140, and doc-tests. The live Codex-auth
  smoke used root session `64e05d17d794` and child session `8e56ce2d38da`;
  the root returned `Paris`, and the child read
  `/tmp/but-codex-agent-parity-smoke.txt` as `agent-parity-smoke-ok`.
- Ten broad real child-agent audits after this slice treated plugin-hook
  ingestion as the only branch-local behavior change. Nine reports completed;
  one child hit the provider turn cap and was recorded as errored. The
  consensus was that plugin hooks are a parity gain, with the expected caution
  that enabled plugin hooks can now affect runtime behavior and must keep
  source/load-order coverage. The remaining material gaps are now concentrated
  in active-turn/thread lifecycle and replay, dynamic MCP/app/plugin callable
  contributors, public event-router style turn buffering, deeper local
  compaction/token lifecycle, cancellable async tool futures with read/write
  gates, richer subagent lifecycle ownership, and review/task lifecycle depth.
- The current mailbox-boundary slice closes a concrete active-turn input queue
  edge. Mailbox-only input that appears after the model has effectively reached
  the answer boundary is no longer appended to the current final response.
  Queue-only mailbox stays pending while the current answer completes; trigger
  mailbox defers the final answer and continues into the next provider turn so
  the mail is handled as its own turn input.
- Verification for the mailbox-boundary slice passed formatting, whitespace,
  Python tests, full Rust workspace tests, and live Codex-auth root plus child
  smokes. The full workspace run passed with browser-use-browser 16 passed plus
  2 ignored browser smokes, CLI 18, core 444, protocol 19, providers 104,
  python-worker 11, store 15, TUI 140, and doc-tests. The live smoke used root
  session `73bbb0b0ca59` and child session `42eaa033ef78`; the root returned
  `Paris`, and the child read `/tmp/but-codex-agent-parity-smoke.txt` as
  `agent-parity-smoke-ok`.
- Ten broad real child-agent audits after this slice treated the mailbox
  answer-boundary behavior as the only branch-local runtime change. The
  consensus was that the focused queue-only and trigger-turn tests cover the
  concrete edge now implemented. The remaining material gap is the larger
  active-turn architecture: generalized `InputQueue`, `TurnState`, and
  `AgentControl` semantics, plus typed history/replay/rollout ownership,
  dynamic callable contributors, compaction/token precision, richer hooks,
  persistent goal state, and subagent control-plane depth.
- The current stdio MCP slice closes the first concrete dynamic callable-tool
  contributor gap. User-configured `[mcp_servers]` entries now discover tools
  through MCP JSON-RPC, register them as namespaced/deferred model tools,
  provide flat fallback tool names for models without namespace support, and
  dispatch `tools/call` through the existing provider-neutral tool loop. The
  model-facing output shape now follows Codex's wall-time/header convention
  for structured/text/image MCP results, while event logs are bounded and MCP
  server stderr is included in error context.
- Verification for the stdio MCP slice passed formatting, whitespace, Python
  tests, full Rust workspace tests, and live Codex-auth root plus child smokes.
  The full workspace run passed with browser-use-browser 16 passed plus 2
  ignored browser smokes, CLI 18, core 452, protocol 19, providers 104,
  python-worker 11, store 15, TUI 140, and doc-tests. The live smoke used root
  session `2a492df679fe` and child session `993caa0d8ff5`; the root returned
  `Paris`, and the child read `/tmp/but-codex-agent-parity-smoke.txt` as
  `agent-parity-smoke-ok`.
- Ten real Codex-auth child audits were run after the stdio MCP slice. Five
  completed with final reports and five hit the provider-turn guard, which is
  recorded as part of the audit result. The completed reports agreed that
  stdio MCP is a real parity gain and that the remaining MCP work is
  architectural: persistent per-session/server connection management,
  discovery diagnostics, remote/OAuth/elicitation/resources, app/plugin
  connector exposure, collision policy, read-only parallel hints, and richer
  raw-vs-model output separation for future hooks/code-mode consumers.
- The current MCP maturity slice closes the concrete MCP-tool heuristics found
  after the first stdio implementation. Small MCP tool sets are now exposed
  directly like Codex, while large sets defer behind `tool_search`; required
  servers fail loudly; Codex-shaped `env_vars` are imported; sanitized name
  collisions are disambiguated; read-only and server-level parallel hints feed
  the existing parallel/predispatch scheduler; and unsupported `original` image
  detail is downgraded before model replay.
- Verification for the MCP maturity slice passed formatting, whitespace,
  Python tests, full Rust workspace tests, and live Codex-auth root plus child
  smokes. The full workspace run passed with browser-use-browser 16 passed
  plus 2 ignored browser smokes, CLI 18, core 454, protocol 19, providers 104,
  python-worker 11, store 15, TUI 140, and doc-tests. The live smoke used root
  session `cf071e265813` and child session `aa8ed0fbb9c8`; the root returned
  `Paris`, and the child read `/tmp/but-codex-agent-parity-smoke.txt` as
  `agent-parity-smoke-ok`.
- Ten broad Codex-auth child audits after the MCP maturity slice produced eight
  final reports and two provider-turn-cap failures. The completed reports did
  not find a new concrete MCP regression. They converged on the same next
  high-impact work: first-class turn/input/control state, typed
  context/history/rollout reconstruction, stronger subagent inheritance and
  lifecycle ownership, dynamic app/plugin/MCP connector inventory, fuller tool
  runtime envelopes, and deeper local compaction/token lifecycle precision.
- The current history/context slice closes a concrete typed-history and
  turn-context gap. `$CODEX_HOME/history.jsonl` now follows Codex's append-only
  JSONL shape with save/none config, max-byte trimming, file identity metadata,
  lookup, and owner-only Unix permissions. CLI and TUI user submissions append
  prompt history; TUI appends asynchronously so the terminal path stays
  responsive.
- The same slice enriches `context.baseline.turn_context` with model request
  capabilities, config/profile/source details, history settings, feature flags,
  MCP server inventory, available model metadata, final-output schema hints,
  and runtime limits. This does not claim full Codex `TurnContext` or state-db
  reconstruction parity, but it gives local replay/compaction/subagent
  diagnostics a much closer client-state snapshot.
- Remaining high-impact client-side gaps after this slice are full TUI message
  history recall/search, typed rollout/state reconstruction, dynamic
  app/plugin/extension tool contributors, async tool futures with cancellation
  and read/write gating, first-class `InputQueue`/`TurnState`/`AgentControl`,
  and deeper compaction/token lifecycle precision.
- Verification for the history/context slice passed the full terminal UI
  definition of done because `crates/browser-use-tui` changed: formatting,
  full Rust tests, Python tests, deterministic setup/account/model/done/
  running/cancelled/browser/history/developer dumps, and the real tmux terminal
  smoke all passed via `scripts/verify-terminal-ui.sh`. The artifact directory
  `/tmp/but-design-loop` was inspected, and no ANSI/bracketed-paste marker leaks
  were found. The live Codex-auth smoke used root session `1e9dff3d2651` and
  child session `6b27af558401`; the root returned `Paris`, and the child read
  `/tmp/but-codex-agent-parity-smoke.txt` as `agent-parity-smoke-ok`.
- Ten broad Codex-auth child audits after this slice all completed. The
  repeated remaining gaps shifted toward the TUI/workbench layer: prompt
  history recall/search, actionable resume/fork/backtrack/session recovery,
  richer goal/status and turn/tool lifecycle visibility, token-usage trend and
  breakdown displays, stronger interrupt/recovery affordances, attachment-rich
  composer state, and exact transcript/history reconstruction.
- The current TUI prompt-history slice closes the highest-confidence
  write-mostly history gap. The composer now reads Codex-shaped
  `$CODEX_HOME/history.jsonl`, combines it with local in-session submissions,
  supports Up/Ctrl+P and Down/Ctrl+N recall with draft restore, keeps multiline
  cursor movement intact, and adds a Ctrl+R reverse-search overlay with
  newest-first unique matching, match cycling, Enter acceptance, and Esc/Ctrl+C
  draft restore.
- The first broad audit wave found several prompt-history edge cases, so the
  slice was tightened before final verification. Normal Up/Down lookup now
  snapshots persistent history metadata and fetches entries lazily; local
  submissions stay separate from later async persistence to avoid duplicate
  recall; adjacent local duplicate submissions are collapsed; and non-empty
  composer history navigation is allowed only when the text exactly matches the
  last recalled entry at a text boundary, matching Codex's safer ownership
  rule. Async command hooks are also skipped like Codex instead of running and
  injecting context.
- Verification for this slice passed the full terminal UI definition of done
  because `crates/browser-use-tui` changed: `scripts/verify-terminal-ui.sh`
  ran formatting, full Rust workspace tests, Python tests, deterministic setup/
  account/model/done/running/cancelled/browser/history/developer dumps, and the
  real tmux smoke. Focused coverage also passed `cargo test -p browser-use-tui
  prompt_history`, `cargo test -p browser-use-tui`, `cargo test -p
  browser-use-core message_history`, and `cargo test -p browser-use-core hook`.
  The artifact directory `/tmp/but-design-loop` was inspected, and no ANSI or
  bracketed-paste marker leaks were found.
- The live Codex-auth smoke used root session `b069942a250f` and child session
  `f6b0a4381917`. The root returned `Paris`; the child read
  `/tmp/but-codex-agent-parity-smoke-final.txt` and returned
  `agent-parity-smoke-ok`.
- Ten broad read-only child audits after final verification all completed in
  two waves. They did not identify a new bounded prompt-history regression.
  Consensus remaining gaps are the larger provider-neutral architecture slices:
  dynamic app/plugin/extension/MCP tool contributors, first-class active-turn
  `InputQueue`/`TurnState`/`AgentControl`, stream-integrated cancellable tool
  futures with read/write gates, typed rollout/history/fork reconstruction,
  exact cumulative turn-diff tracking, deeper compaction/token lifecycle
  precision, persistent MCP/session lifecycle, richer skill/plugin/review
  manager depth, and multi-environment tool routing.
- The current turn-diff slice closes a concrete part of Codex's
  `TurnDiffTracker` behavior. The file-tool runtime now keeps an in-memory
  turn-scoped tracker for committed `apply_patch` changes, reset at the start of
  each model turn and rooted at the git worktree root when available.
  `turn.diff` events from patches are cumulative and exact when the committed
  deltas are exact: add-then-update is rendered as one add, delete-then-readd
  becomes one update, add-then-delete clears the diff with an empty cumulative
  event, and move-overwrite captures both the source delete and destination
  update. Once an exact cumulative patch diff has been emitted for the turn, the
  broader git-worktree fallback no longer appends a competing `turn.diff`;
  shell/worktree snapshots still remain the fallback for non-`apply_patch`
  mutations.
- Focused verification for this slice passed `cargo test -p browser-use-core
  apply_patch` and `cargo test -p browser-use-core turn_git_diff`, including
  regressions for per-turn reset, git-root display paths, net-empty patch diffs,
  and tracked-file git-fallback suppression. Full workspace verification also
  passed (`cargo fmt --check`, `git diff --check`, Python tests, and
  `cargo test`). The live Codex-auth smoke used root session `db6bce744f5a` and
  child session `3909b3887310`; the root returned `Paris`, and the child read
  `/tmp/but-codex-agent-parity-smoke-turndiff-final.txt` as
  `agent-parity-smoke-ok`.
- Ten broad read-only child audits after final verification completed in two
  waves and were closed. They did not find a bounded regression in the
  cumulative turn-diff slice. Their consensus remaining gaps are the larger
  provider-neutral runtime systems: dynamic MCP/app/plugin/extension tool
  contributors and persistent MCP sessions, stream-integrated cancellable tool
  futures with read/write gates, first-class active-turn
  `InputQueue`/`TurnState`/`AgentControl`, typed rollout/history/fork
  reconstruction, token-window/compaction precision, goal runtime state depth,
  structured review/user-shell surfaces, hook trust/handler breadth,
  skill/plugin manager side effects, and multi-environment tool routing.
- The current MCP resource slice closes another concrete dynamic-tool gap.
  When MCP servers are configured, the registry now exposes Codex-shaped
  `list_mcp_resources`, `list_mcp_resource_templates`, and `read_mcp_resource`
  tools. Dispatch sends stdio MCP `resources/list`, `resources/templates/list`,
  and `resources/read` requests, supports single-server cursors, aggregates
  all-server listings with server-tagged entries, records `mcp.resource_result`
  sidecars, and marks the resource tools as read-only/parallel-safe. This gives
  the model a portable way to inspect MCP-provided context without relying on
  browser behavior or OpenAI-only server features.
- Focused verification for the MCP resource slice passed
  `cargo test -p browser-use-core mcp_resource -- --nocapture`, covering tool
  specs/read-only dispatch flags, raw stdio resource list/template/read calls,
  pagination aggregation, and provider-loop model-visible outputs. It does not
  claim persistent MCP-session parity; the local MCP path still launches per
  operation and still lacks Codex's streamable HTTP/OAuth/elicitation/app/plugin
  connector lifecycle.
- Full verification for this slice passed `cargo fmt --check`,
  `git diff --check`, `uv run --with pytest python -m pytest -q`, and
  `cargo test`. The Rust workspace results were browser-use-browser 16 passed
  plus 2 ignored browser smokes, CLI 18, core 466, protocol 19, providers 104,
  python-worker 11, store 15, TUI 143, and doc-tests. The live Codex-auth smoke
  used root session `7a8cbb74fe0c` and child session `03275ba33199`; the root
  returned `Paris`, and the child read
  `/tmp/but-codex-agent-parity-smoke-mcpres-final.txt` as
  `agent-parity-smoke-ok`. No TUI behavior changed, so
  `scripts/verify-terminal-ui.sh` was not rerun for this slice.
- Ten broad read-only child audits after the MCP resource slice all completed.
  They agreed the resource tools are a useful local parity improvement and did
  not report a bounded regression. The consensus remaining gaps are larger
  provider-neutral runtime systems: dynamic per-turn tool contributors for
  plugins/apps/extensions/deferred tools, persistent MCP sessions, unified
  cancellable async tool execution with read/write gates, first-class
  `InputQueue`/`TurnState`/`AgentControl`, typed response-item history and
  rollout/fork/replay reconstruction, compaction/token-window precision,
  multi-environment tool routing, hook engine/trust depth, plugin/skill manager
  side effects, and fuller goal/review/user-shell lifecycle state. The most
  repeated next implementation order was persistent stdio MCP manager, dynamic
  tool contributor planning, active-turn/tool cancellation runtime, then typed
  history reconstruction.
- The current MCP session slice closes the persistent-stdio part of that gap.
  Local MCP discovery, tool calls, resource listing, resource templates, and
  resource reads now reuse an initialized stdio server process keyed by the
  session id plus stable server config, matching Codex's managed-client behavior
  for stateful MCP servers without sharing state across unrelated sessions. The
  first broad audit wave caught the initial process-global cache risk, so the
  patch now routes normal agent MCP calls through session-scoped helpers and
  folds MCP shutdown into the same session/subtree/process cleanup path used for
  background terminal sessions. Operations are serialized per server, request
  ids keep increasing across calls, newest stderr is still attached to failures,
  and timeouts or transport failures drop the cached session. Focused MCP tests
  include a stateful Python server whose counter must advance from `count:1` to
  `count:2` in one session, start at `count:1` in another session, and reset to
  `count:1` after explicit cleanup. Remaining MCP work is streamable
  HTTP/OAuth/elicitation, app/plugin connector lifecycle, richer startup status
  events, retry-on-fresh-transport nuances, same-server parallel throughput, and
  the broader dynamic tool contributor graph.
- Full verification passed after the session-scoped MCP slice. The terminal
  verifier ran formatting, the full Rust workspace, Python tests, deterministic
  UI dumps, and the real tmux terminal smoke; `/tmp/but-design-loop` was
  inspected and scans found no ANSI escape or bracketed-paste marker leaks. A
  final live Codex-auth smoke used root session `1b346f802c2e`, which returned
  `Paris`, and child session `89442bdb6ff2`, which read `AGENTS.md` as
  `Agent Notes`.
- Ten broad read-only child audits completed after the MCP work. The first wave
  found the initial process-global cache risk; that drove the session-scoping,
  cleanup, newest-stderr, and stale-transport recovery fixes. The final wave
  agreed persistent stdio MCP is now a useful local parity improvement, while
  the remaining MCP deltas are lifecycle depth, same-server parallel throughput,
  HTTP/OAuth/elicitation, startup/status/provenance, and app/plugin connector
  integration.
- The current plugin-MCP slice connects enabled local Codex plugin bundles to
  the same local stdio MCP runtime instead of only showing plugin MCP names in
  prompt context. Wrapped `.mcp.json` files, flat server-map files, manifest
  `mcpServers` paths, relative `cwd` normalization, OAuth key normalization,
  and explicit-config precedence are covered by tests. A runtime regression
  proves a plugin-provided stdio MCP server can expose and execute
  `mcp__sample__echo_tool` through the provider loop. This deliberately does
  not claim app connector, streamable HTTP, OAuth, elicitation, plugin install,
  or marketplace sync parity.
- Verification for the plugin-MCP slice passed formatting, whitespace, Python,
  and full Rust workspace checks. The live Codex-auth smoke used root session
  `8a3915043d32`, which returned `Paris`, and child session `291d1d6b0884`,
  which read `AGENTS.md` as `Agent Notes`. No TUI behavior changed, so the
  terminal verifier was not rerun for this slice.
- Ten broad read-only audits after the plugin-MCP slice agreed that supported
  local stdio plugin MCP runtime exposure is now closed. They did not find a
  bounded parser/runtime regression in this slice. Their consensus moved the
  next work back to architecture: a dynamic tool/contributor graph including
  discoverable plugin/install tools, a cancellable stream-time tool runtime
  with read/write gates and a shared turn-diff tracker, a first-class
  `InputQueue`/`TurnState`/`AgentControl`, typed response-item/context history,
  local compaction/token-window precision, MCP/tool-output fidelity, hook trust
  and handler depth, structured review task lifecycle, multi-environment
  routing, and exact diffs beyond `apply_patch`.
- The repeated next high-impact gaps are not MCP-specific: per-turn dynamic
  tool contributor/router planning, a unified cancellable tool runtime with
  read/write gates and abort outputs, first-class active-turn
  `InputQueue`/`TurnState`/`AgentControl`, typed response-item/context history
  for replay/fork/resume/compaction, and deeper local compaction/token-window
  precision.

## Definition of Done

The experiment is complete when no material agent-quality behavior differences
remain between this agent and Codex, except for explicitly documented product
wiring differences that do not reduce agent quality.
