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
will be handled by a separate goal.

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

The working rule is simple: if a difference can affect agent quality, it stays
open until it is either fixed or deliberately rejected with evidence. If several
related differences can be fixed in the same loop, fix all of them in that loop.

## Current State

- Branch: `agent-gap-zero`
- Gap log: `docs/agent-gap-log.md`
- Status: a 10-scope Codex-auth closure audit on 2026-05-24 found the gap is
  not closed. Prompt/context alignment is substantially improved, `apply_patch`
  has verified-write semantics for common Codex patch behavior plus the first
  Codex-shaped writable-root and protected-metadata safety slice, and the shell
  command path now has a first Codex-shaped safety slice for read-only
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
  1 MiB head/tail buffer instead of an unbounded vector.
  Compaction now preserves the canonical
  startup context instead of replacing it with only a summary. AGENTS discovery
  now reads Codex's project-doc config knobs from Codex-home `config.toml` and
  trusted project-local `.codex/config.toml`, including the final legacy
  managed-config and macOS managed-preferences precedence layers.
  Codex-style `developer_instructions` are now loaded from that same config
  stack and appended to the aggregated developer context alongside the
  no-approval permissions instructions. Environment context now covers
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

The latest batch closed the represented MultiAgentV1 skill/reference/resume
cluster in G-033.

- V1 spawned agents are now id-only like Codex. Local no longer fabricates a
  task path for `multi_agent_v1.spawn_agent`, and completion notifications use
  the child `agent_id` as the `agent_path` field Codex exposes for legacy v1.
- Structured `skill` items now behave like Codex for the represented path:
  they stay in the saved typed input for UI/state, are removed from ordinary
  user content, and replay as separate user-role `<skill>` context by loading
  the referenced `SKILL.md`.
- V1 child-completion mail now enters the parent model as a direct user-role
  `<subagent_notification>` contextual fragment instead of an assistant
  inter-agent envelope.
- V1 `resume_agent` now rejects invalid or missing ids as tool errors, reopens
  cancelled targets, and restores descendants whose own edge remained open.
  `close_agent` now marks only the target edge closed while cancelling active
  descendants, so parent traversal hides the closed subtree but target resume
  can reopen still-open descendants.
- The `non_code_mode_only`/`DirectModelOnly` audit found no current local
  code-mode runtime. A guard test records that these tools remain visible in
  normal model mode, and the runtime split stays deferred until this repo
  actually has code mode.
- Remaining multi-agent gaps are mention/plugin/app context injection, exact
  app-server collaboration/input-queue events, CLI legacy spawn/resume/wait
  commands, full rollout persistence semantics, arbitrary role config keys such
  as sandbox/skills/tools, and code-mode-only runtime behavior if code mode is
  added.
- Remaining large non-multi-agent gaps are websocket transport/fallback/
  `response.processed`, richer effective-config provenance and doctor/status
  displays, full app-server thread-config protocol surfaces beyond the
  represented core event, and broader config-layer stack behavior.

The previous batch closed the represented MultiAgentV1 typed-input and deferred
tool-discovery cluster in G-033.

The previous batch closed the represented multi-agent v2 config, mailbox,
completion, concurrency, and usage-hint cluster in G-033.

The previous batch before that closed the Bedrock AWS SigV4/profile-auth
execution slice in G-032. The earlier Bedrock batch closed the static-catalog
and bearer-token Mantle execution slice in G-032.

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
  refresh retry, AWS/Bedrock execution, full built-in provider merge semantics,
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
  `wire_api = "responses"` validation. Command-backed auth, AWS/Bedrock
  execution, full built-in merge semantics, websocket fields, and
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
  payload rule and dedupes repeated custom tool calls if a local mock/provider
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
- Added trusted project-local AGENTS config layering: `.codex/config.toml`
  may override `project_doc_max_bytes` and `project_doc_fallback_filenames`
  after the project is trusted in the user config. Project-local
  `project_root_markers` is ignored for AGENTS discovery, matching Codex's
  non-circular root-selection rule.
- Added Codex's file-backed AGENTS config precedence for the implemented
  layers: Unix system config, Codex-home user config, trusted project config,
  then legacy `managed_config.toml`. Managed config can override
  `project_root_markers` for AGENTS discovery because Codex merges non-project
  layers for that decision after the final stack exists.
- Added Codex profile-v2 AGENTS config layering for selected profiles:
  `$CODEX_HOME/<name>.config.toml` is treated as a second user layer above base
  user config, can override project-doc settings and `project_root_markers`,
  and can participate in project trust. The local runner also validates plain
  profile names and rejects selected legacy `profile`/`[profiles.<name>]`
  conflicts like Codex.
- Added Codex `-c/--config key=value` session override behavior for AGENTS
  config: direct CLI/provider runs parse TOML values with raw-string fallback,
  build dotted-key override tables, use those overrides for pre-project
  root/trust discovery, apply them above trusted project config, and keep
  managed config higher precedence.
- Added Codex legacy managed-config MDM behavior for AGENTS config: Unix
  managed config defaults to `/etc/codex/managed_config.toml` outside tests,
  and macOS `com.openai.codex:config_toml_base64` managed preferences are
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
  fallback, process id allocation/pruning, and background completion are now
  partially aligned. Still open: shell-mode breadth, pause-aware waits, final
  end-event transcript, dynamic truncation policy, code-mode JSON, and full
  parser parity.
- Patch semantics: live progress snapshots from streaming custom-tool deltas,
  committed-delta reporting after runtime write/remove failures, structured
  app-server patch lifecycle events, and sandbox-dependent hardlink/path
  protections.
- Collaboration and Plan mode: exact Default/Plan collaboration templates,
  config-gated injection, later-turn developer updates, Plan-mode `update_plan`
  rejection, local TUI/CLI mode controls, the `request_user_input` answer path,
  structured option/notes/unanswered handling, active and completed question
  rendering, Plan-mode-only proposed-plan rendering, active-turn `/plan` safety,
  and the Plan-mode medium reasoning preset are now implemented. Remaining open
  work is app-server-compatible collaboration/request lifecycle events and
  settings broadcasting, exact `turn_id` answer semantics, pending
  replay/filtering, delegated/MCP compatibility paths, and full feature-flag
  breadth such as the default-mode request-user-input flag.
- Turn lifecycle and finalization: interrupted-turn replay now includes Codex's
  model-visible `<turn_aborted>` marker and `aborted` missing-call outputs, and
  local sidecars now cover Codex-shaped task start/complete/abort,
  final-answer extraction, retry stream errors, consumed token totals,
  recomputed active-context token counts, and latest rate-limit snapshots.
  Still open: fully typed app-server lifecycle surfaces,
  config-gated/developer-role interrupt marker variants, remote/v2 compaction
  item handling, exact typed token estimation, and resume/fork context breadth.
- Full config/model and multi-agent systems: the represented model catalog
  substrate now drives request metadata, tool capability gates, spawn-agent
  validation, tool-output truncation, remote/cache refresh, TUI picker rows,
  TUI `--profile`/`-c` runtime propagation, and Bedrock static catalog plus
  bearer-token and AWS SigV4 Mantle execution. Still open: complete
  `ConfigLayerStack`, thread snapshots, v1 multi-agent tools, true app-server
  mailbox/input-queue semantics, and app-server bridge events.

## Still Open

- Exact Codex instruction layering beyond the now-aligned no-approval
  permissions fragment, simple AGENTS plus environment startup context,
  config-backed `developer_instructions`, config-backed base instructions,
  session-persisted base instructions, compaction reinjection, and
  fallback-provider priority mapping. Still open: separate developer
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
  fallback, `response.processed`, app-server thread-settings/session-config
  notification surfaces beyond the core
  `session.config_snapshot`, and app-server lifecycle/token replay surfaces.
- Tool schema parity beyond the now-closed direct v2 terminal schema surface:
  v1 namespaced multi-agent tools, true app-server mailbox behavior, and
  non-schema runtime semantics tracked in the exec/patch/multi-agent rows.
- Full runtime shell approval, sandbox, and execpolicy behavior: rule files,
  prefix-rule approvals, feature-enabled additional-permission normalization
  and approval, network denial handling, full tree-sitter parser parity beyond
  the tested Unix plain-command edge cases, Codex shell-mode breadth including
  PowerShell/Windows heuristics, pause-aware wait deadline extension, dynamic
  Codex's separate whole-process transcript buffer/final unified-exec end-event
  surface, code-mode JSON if this terminal adds code-mode, and
  exact prompt-vs-allow behavior for non-dangerous unmatched commands.
- Full patch sandbox execution, protected-path approval prompts, and approval
  decision logic are intentionally out of scope for this experiment unless the
  product later needs them. Still open for patch quality: hardlink-escape
  protection that depends on sandboxing, streaming patch-input progress events
  from `response.custom_tool_call_input.delta`, committed-delta reporting for
  runtime write/remove failures, and structured patch progress/end events.
  Dirty-worktree ownership remains prompt-level in Codex rather than a runtime
  patch guard.
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
  default resumes, and Bedrock static catalog plus bearer-token and AWS SigV4
  Mantle execution are represented. Still open: full `ConfigLayerStack`,
  thread/project layer behavior, harness/API `ConfigOverrides.base_instructions`,
  full app-server provider/model snapshot protocol surfaces on resume/fork
  beyond the represented core event, richer Codex doctor/status-style config
  displays, unsupported-verbosity warning
  telemetry, and deeper app/MCP/code-mode output surfaces not represented by
  this terminal runtime.
- Plan-mode collaboration behavior beyond the implemented collaboration
  instructions, mode controls, request-input answer UI, proposed-plan rendering,
  and reasoning preset: full app-server collaboration/request events, exact
  `turn_id` response semantics, pending interactive replay/filtering,
  delegated/MCP compatibility paths, and feature-flag breadth.
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
  config-gated/developer-role interrupt marker variants, remote/v2 compaction
  item handling, exact resume/fork `TurnContextItem` persistence, full typed
  auth-refresh transport retry, exact typed token estimation, and full
  app-server compaction/token-window lifecycle.

## Definition of Done

The experiment is complete when no material agent-quality behavior differences
remain between this agent and Codex, except for explicitly documented product
wiring differences that do not reduce agent quality.
