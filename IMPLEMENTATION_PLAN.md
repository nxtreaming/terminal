# IMPLEMENTATION_PLAN.md — phased, parallel-safe, sub-agent execution plan

> Execution plan for the rework described in `REARCHITECTURE.md`.
> Designed to be driven by **many sub-agents in parallel** — but *safely* (agents never edit the same file).
> Governing rule: **EXTREME PARITY** with codex on engine features; preserve only the intentional divergences listed in `REARCHITECTURE.md` ("Guiding principle").
> Status: **plan only — do not implement yet.** This is the map we execute when we decide to go.

---

## Implementation status — check off as each piece is implemented **and tested**

Legend: `[ ]` not started · `[~]` in progress · `[x]` implemented **and** parity/e2e tests green. A phase is done only when every box under it (including its Gate) is `[x]`. Nothing gets `[x]` until it runs and passes tests (Rule 8). Keep this list in sync as WPs land.

**Phase 0 — Carve**
- [~] 0.1 Module carve — **~16 cohesive modules extracted** (constants, images, auth, goals, memory_citations, rollback, codex_headers, review, config_overrides, persistence, prompts, tool_output, request_user_input, assistant_markup, token_usage), lib.rs 47.3k→**44.3k**, 488 tests green. **Plateau:** the remaining tangled core (providers_glue, context, compact, session, events, agents_md, hooks, skills, plugins, subagents, turn) can't be cleanly split by pure code-motion — it needs the async + interface rework it's a prerequisite for. Finish the rest *during* the async migration (Phase 2).
- [ ] 0.2 Freeze interfaces (provider/route, ToolRuntime/Approvable/Sandboxable, context, store, errors)
- [ ] 0.3 Parity-test harness (cassettes, fixtures, corpus scaffold)
- [ ] **Gate 0** — compiles (sync), modules carved, interfaces frozen, harness runs

**Phase 1 — `browser-use-llm`**
- [x] 1.1 `schema/` (LlmRequest/Message/ContentPart/LlmEvent/Usage/ids/options/error) — 5 round-trip tests green
- [~] 1.2 `route/` traits + client + Lifecycle + ToolStream — **Lifecycle + ToolStream + SSE framing done** (16 tests; all pure/sync). `route/` Protocol/Endpoint/Auth traits + async client still pending.
- [x] 1.3 `protocols/openai_responses` — build_body + SSE decoder via Lifecycle/ToolStream; 6 fixture tests
- [x] 1.4 `protocols/openai_chat` (Ollama/OpenRouter/DeepSeek/Fireworks) — 5 fixture tests (tool calls correlated by stream index)
- [x] 1.5 `protocols/anthropic_messages` — Messages wire format (system-block array, thinking-signature round-trip, event-named SSE); 9 fixture tests. *(Claude-Code OAuth is auth, not wire format — lands with the auth/client layer.)*
- [ ] 1.6 `route/executor` (retry / rate-limit / redaction)
- [ ] 1.7 `providers/` facades + openai-compatible `{provider,base_url}` table
- [ ] 1.8 `tool.rs` / `tool_runtime.rs`
- [ ] 1.9 Drop codex/ChatGPT backend (de-codex)
- [ ] **Gate 1** — 3 protocols pass cassette parity; live smoke (OpenAI/Anthropic/Ollama); codex backend gone

**Phase 2 — Async spine + orchestrator seam**
- [ ] 2.1 Async turn loop (codex parity)
- [ ] 2.2 Context manager + **real** token accounting + context-message injection
- [ ] 2.3 ToolOrchestrator + runtime/approval/sandbox seam (sandbox=None stub)
- [ ] 2.4 Session/resume async over SQLite (write-sink)
- [ ] **Gate 2** — full async turn end-to-end through orchestrator; persist + resume

**Phase 3 — Tools → parity**
- [ ] 3.1 shell + unified_exec
- [ ] 3.2 apply_patch (+ turn_diff_tracker)
- [ ] 3.3 view_image (KEEP sync/serial)
- [ ] 3.4 update_plan
- [ ] 3.5 request_user_input
- [ ] 3.6 tool_search + deferred exposure
- [ ] 3.7 web_search
- [ ] 3.8 browser tools adapter
- [ ] 3.9 python tool adapter
- [ ] 3.10 mcp tool dispatch
- [ ] **Gate 3** — every tool parity/divergence-tested

**Phase 4 — Subsystems → parity**
- [ ] 4.1 Compaction (codex parity, model-based; **no-LLM path removed**)
- [ ] 4.2 Context-message system (reference_context alignment)
- [ ] 4.3 MCP transports (HTTP/SSE/OAuth/elicitation; fix plugin-oauth bug)
- [ ] 4.4 Subagents (roles-as-config, depth limits, **event-notify** mailbox)
- [ ] 4.5 Goals/budget parity
- [ ] 4.6 Skills & plugins parity (+ install/elicitation)
- [ ] 4.7 Hooks (PermissionRequest event + Prompt/Agent handlers)
- [ ] 4.8 Prompts (de-brand; selection parity)
- [ ] 4.9 Rollout/resume hardening (archival/fork/truncation over SQLite)
- [ ] **Gate 4** — subsystem parity tests green; no-LLM compaction gone

**Phase 5 — Safety substrate**
- [ ] 5.1 OS sandboxes (seatbelt/landlock+seccomp/bwrap/windows) + process-hardening
- [ ] 5.2 execpolicy (Starlark) + amendments + command_canonicalization
- [ ] 5.3 Network proxy/MITM + deferred network approval
- [ ] 5.4 Guardian (LLM reviewer, circuit breaker, fork reviewer)
- [ ] 5.5 Wire approvals into orchestrator; flip sandbox None→real; PermissionRequest precedence
- [ ] **Gate 5** — commands sandboxed; approvals/guardian enforced; escalation works

**Phase 6 — Advanced parity**
- [ ] 6.1 code_mode (V8 tools-as-code)
- [ ] 6.2 agent_jobs (CSV fan-out)
- [ ] 6.3 shared-service subthreads (codex_delegate equiv)
- [ ] 6.4 memories ingestion/storage
- [ ] 6.5 connectors (OAuth MCP) + session prewarm

**Cross-cutting**
- [ ] De-codex audit (no residual `codex`/`CODEX_`/`oai`/`chatgpt` except intentional)
- [ ] Final full e2e pass (browser + tools + compaction + subagents + resume, against a live model)

---

## 0. How this plan is meant to be run

- The unit of work is a **work package (WP)**. Each WP = one sub-agent task, going deep / first-principles.
- WPs are grouped into **phases** (by dependency) and, within a phase, into **parallel groups** (WPs that touch disjoint files can run at the same time).
- **There is no human in the loop.** The **orchestrating agent** drives everything: it runs each phase as a **wave** — spin up the group's agents in parallel, review their design notes, let them implement, run the **phase gate** (integration + parity + e2e tests), merge, then start the next wave. It also does the serial work (the Phase 0 carve) itself.
- **The goal is completeness, not speed.** Parallelism here is for clean isolation and depth — *not* to finish fast. Every subsystem must be fully implemented, **run, and tested**; a phase is not done until it actually works.

### Safe-parallelism rules (non-negotiable)
1. **Disjoint files only.** Two agents may run concurrently *only* if their owned-file sets do not intersect. Every WP below lists **Owns** (exact files/dirs).
2. **The carve gates everything.** While the engine is one 47k-line `lib.rs`, parallel edits collide. **Phase 0 (the split) must complete before high-parallelism phases.** Phase 0 itself is low-parallelism.
3. **Interfaces first, frozen.** Shared types/traits/errors are built serially, frozen as contracts, then feature agents code against them (not against each other).
4. **Worktree isolation for concurrent coders.** Each parallel agent works in its own git worktree (or branch); the **orchestrating agent** merges at the phase gate. (Workflow `isolation: "worktree"`.)
5. **Each WP de-brands its own files.** The de-codex rename (REARCHITECTURE §4) is done *inside each WP's owned files* — never as a global sweep mid-phase (that would touch everyone's files). A final audit WP catches stragglers.
6. **A phase isn't done until its gate is green and merged.** No starting a dependent wave on un-merged work.
7. **Tests first, commit first.** Each WP writes + **commits its parity/e2e tests before** implementing the feature, then commits working increments frequently (don't batch a whole subsystem into one commit). The Phase 0 carve commits per extraction step.
8. **Run it, don't just compile it.** Acceptance = the behavior runs and tests pass against a live model (see §5 Testing) — not that it builds.

### Per-WP design note (every agent produces this BEFORE coding — first principles)
This is the "go extremely deep into every single thing" contract. Reviewed by the coordinator before implementation starts:

1. **Purpose** — what is this actually *for*? (the agent-/user-facing behavior it provides)
2. **Mechanism** — how does **codex** do it (types, flow, file:line)? How do we do it **today**?
3. **Heuristics** — enumerate codex's exact heuristics/constants/thresholds/decision-rules (the real numbers).
4. **Heuristic decision** — which value/behavior do we pick? **Default = match codex exactly.** Any divergence must be one of the sanctioned ones (REARCHITECTURE "Guiding principle") *or* explicitly justified here.
5. **Parity gap** — precisely what differs/missing today vs codex.
6. **Tasks** — concrete implementation steps.
7. **Parity tests** — tests that *prove* behavior matches codex (same triggers → same outputs → same limits). "Feature exists" is not acceptance; "behaves identically" is.
8. **Owns** — exact file list (collision-safety).

---

## 1. Target workspace layout (what the carve produces)

```
crates/
  browser-use-protocol/      # exists — shared wire/event types (de-branded)
  browser-use-llm/           # NEW — provider/protocol layer (opencode design)
  browser-use-core/          # agent engine, split into submodules:
      src/turn/              #   the async turn loop
      src/context/           #   context manager (REAL tokens) + context-message injection
      src/compact/           #   compaction (codex parity, model-based)
      src/session/           #   session lifecycle, resume, rollback/fork (over store)
      src/orchestrator/      #   ToolOrchestrator + ToolRuntime/Approvable/Sandboxable seam
      src/tools/<tool>.rs    #   one file per tool handler
      src/goals/             #   goals/budget
      src/subagents/         #   multi-agent v1+v2
      src/skills/            #   skills + plugins
      src/hooks/             #   hooks engine
      src/prompts/           #   prompt assembly + catalog
  browser-use-mcp/           # NEW — MCP client + transports (extracted from mcp.rs)
  browser-use-sandbox/       # NEW — OS sandboxes + execpolicy + network proxy + hardening
  browser-use-guardian/      # NEW — LLM approval reviewer
  browser-use-store/         # exists — SQLite (extend, keep)
  browser-use-browser/       # exists — CDP/JS harness (black box)
  browser-use-python-worker/ # exists
  browser-use-tui/           # exists
  browser-use-cli/           # exists
```
Engine stays one crate (`browser-use-core`) with **submodules** — submodule files give enough isolation for one-agent-per-file parallelism without the cost of extracting a dozen crates. Clearly separable concerns (`llm`, `mcp`, `sandbox`, `guardian`) become their own crates.

---

## 2. Dependency graph (phase level)

```
Phase 0  Carve + interfaces            ── gates everything
   │
   ├── Phase 1  browser-use-llm core   ──┐
   │                                     ├── Phase 2  Async spine + orchestrator seam
   └───────────────────────────────────┘        │
                                                 ├── Phase 3  Tools → parity   (HIGH parallel)
                                                 ├── Phase 4  Subsystems → parity (HIGH parallel)
                                                 │        │
                                                 └── Phase 5  Safety substrate (needs orchestrator seam)
                                                          │
                                                       Phase 6  Advanced parity (HIGH parallel)
```

---

## 3. Phases

Legend — **P** = parallelism of the phase. Each WP: **Owns** (files) · **Dep** (prereqs) · **Group** (parallel cohort).

### Phase 0 — Carve the territory  ·  P = LOW (gating)
Goal: modular layout + frozen interfaces, **behavior-preserving, still sync**. This is the bottleneck; do it carefully with frequent commits. Do **not** parallelize across `lib.rs`.

| WP | Owns | Dep | Group |
|---|---|---|---|
| 0.1 Module/crate skeleton + mechanical move of code into submodules/crates (no behavior change, sync) | whole workspace (esp. `core/src/lib.rs` → submodules) | — | **solo** |
| 0.2 Freeze interfaces: `ModelProvider`/route traits, `ToolRuntime`/`Approvable`/`Sandboxable`, context-manager trait, store trait, error enums | `core/src/*/mod.rs` trait files, `browser-use-llm/src/route/*.rs` (signatures only) | 0.1 | after 0.1 |
| 0.3 Parity-test harness: provider cassettes, fixture framework, codex-parity corpus scaffolding | `tests/parity/**` (new) | — | parallel w/ 0.2 |

**Gate 0:** workspace compiles (sync), all subsystems live in their own module/crate, public interfaces frozen, parity harness runnable. *Now one-agent-per-module is collision-safe.*

> Sequencing tip: 0.1 may be split into ordered extraction commits by subsystem (turn → context → compact → tools → goals → subagents → skills → hooks → mcp → store), but each commit is serial (single owner) because they all touch `lib.rs`.

### Phase 1 — `browser-use-llm` provider core  ·  P = MEDIUM
Foundation (serial), then protocols (parallel). See REARCHITECTURE §3.

| WP | Owns | Dep | Group |
|---|---|---|---|
| 1.1 `schema/` — `LlmRequest`/`Message`/`ContentPart`/`LlmEvent`/`Usage`/`ids`/`options`/`error` | `llm/src/schema/**` | 0.2 | **foundation (serial)** |
| 1.2 `route/` traits + `client` + `Lifecycle` + `ToolStream` helpers | `llm/src/route/**`, `llm/src/protocols/utils/**` | 1.1 | foundation (serial) |
| 1.3 `protocols/openai_responses.rs` | that file | 1.2 | **wave A (parallel)** |
| 1.4 `protocols/openai_chat.rs` (Ollama/OpenRouter/DeepSeek) | that file | 1.2 | wave A |
| 1.5 `protocols/anthropic_messages.rs` (+ Claude-Code OAuth, identity/tool-name canon, system-block, freeform→JSON downgrade) | that file + `providers/anthropic.rs` | 1.2 | wave A |
| 1.6 `route/executor.rs` — retry/backoff, rate-limit headers, secret redaction | that file | 1.2 | wave A |
| 1.7 `providers/` facades + OpenAI-compatible `{provider,base_url}` table | `llm/src/providers/**` | 1.2 | wave A |
| 1.8 `tool.rs`/`tool_runtime.rs` (define-once tools → per-protocol lowering) | those files | 1.2 | wave A |
| 1.9 **Drop the codex/ChatGPT backend** (de-codex) | remove `CodexResponsesProvider` + its headers/OAuth/env | 0.1 | wave A |

**Gate 1:** all three protocols pass cassette parity tests; live smoke against OpenAI, Anthropic (key + OAuth), Ollama; codex backend gone.

### Phase 2 — Async spine + orchestrator seam  ·  P = LOW–MEDIUM
The engine becomes async; everything routes through one orchestrator. Tightly coupled WPs — coordinate.

| WP | Owns | Dep | Group |
|---|---|---|---|
| 2.1 Async turn loop — **codex parity**: unbounded `loop{}` on `needs_follow_up`, `CancellationToken` tree, `FuturesOrdered` tool scheduling, abort/interrupt | `core/src/turn/**` | 1.x, 0.2 | spine (coordinate w/ 2.3) |
| 2.2 Context manager — **REAL** token accounting (per-provider `Usage`), history assembly/normalization, context-message injection | `core/src/context/**` | 1.1 | parallel w/ 2.4 |
| 2.3 `ToolOrchestrator` + `ToolRuntime`/`Approvable`/`Sandboxable` seam (sandbox=`None` stub, auto-approve); unify shell/exec/apply_patch/mcp routing | `core/src/orchestrator/**` | 0.2 | spine (coordinate w/ 2.1) |
| 2.4 Session/resume async over SQLite — keep store; wire async load/replay | `core/src/session/**`, `store` async API | 0.2 | parallel w/ 2.2 |

**Gate 2:** an async agent runs a full turn end-to-end through the orchestrator (sandbox off), dispatches tools, persists + resumes a session.

### Phase 3 — Tools → parity  ·  P = HIGH (one agent per tool file)
All depend on Phase 2; mutually disjoint → run as one big wave.

| WP | Owns | Parity target |
|---|---|---|
| 3.1 shell + unified_exec | `core/src/tools/shell.rs`, `unified_exec*.rs` | persistent PTY sessions, stdin, timeouts, truncation; route through orchestrator |
| 3.2 apply_patch | `core/src/tools/apply_patch.rs` | V4A format + `turn_diff_tracker` parity |
| 3.3 view_image | `core/src/tools/view_image.rs` | **KEEP sync/serial** (intentional) — test that it never runs parallel with browser actions |
| 3.4 update_plan | `core/src/tools/plan.rs` | spec + semantics parity |
| 3.5 request_user_input | `core/src/tools/request_user_input.rs` | parity |
| 3.6 tool_search + deferred exposure | `core/src/tools/tool_search.rs` | bm25 + deferral threshold parity |
| 3.7 web_search (hosted) | `core/src/tools/web_search.rs` | parity |
| 3.8 browser tools adapter | `core/src/tools/browser.rs` | thin adapter over `browser-use-browser` (divergence, keep) |
| 3.9 python tool adapter | `core/src/tools/python.rs` | adapter over `python-worker` (divergence, keep) |
| 3.10 mcp tool dispatch | `core/src/tools/mcp.rs` | calls `browser-use-mcp` |

**Gate 3:** every tool has a parity (or divergence) test passing.

### Phase 4 — Engine subsystems → parity  ·  P = HIGH (one agent per module)
All disjoint modules; depend on Phase 2 (+3 for tool-touching ones). The core "EXTREME PARITY" wave.

| WP | Owns | Parity target |
|---|---|---|
| 4.1 **Compaction** | `core/src/compact/**` | **codex parity, model-based only** (remove no-LLM dump): triggers on **real** token usage (Total + BodyAfterPrefix + full-window), exact summary prompt, preserve full `COMPACT_USER_MESSAGE_MAX_TOKENS=20_000` user budget, byte-identical summary prefix, context-window-exceeded drop-oldest handling |
| 4.2 Context-message system | `core/src/context/messages.rs` | **decision:** keep our `name`-tagged messages vs adopt codex `reference_context` diff — pick one, justify; match codex injection points/content |
| 4.3 MCP transports | `browser-use-mcp/**` | add HTTP/SSE/streamable + OAuth + elicitation; honor plugin `type`/`oauth` (fix the silently-ignored bug); keep stdio |
| 4.4 Subagents | `core/src/subagents/**` | role-as-config-layer, spawn-depth limits, mailbox semantics; **event-notify** (in-memory) instead of the 50ms SQLite poll — messages still *dumped* to SQLite for the record, never polled |
| 4.5 Goals/budget | `core/src/goals/**` | audit accounting formula + steering events to parity |
| 4.6 Skills & plugins | `core/src/skills/**` | discovery/budgeting/mentions parity + plugin install/elicitation |
| 4.7 Hooks | `core/src/hooks/**` | add `PermissionRequest` event + `Prompt`/`Agent` handler kinds |
| 4.8 Prompts | `core/src/prompts/**` | de-brand; parity with codex prompt selection; keep browser preamble + interaction-skills + sync-image notes |
| 4.9 Rollout/resume hardening | `core/src/session/**` (≠4.x), `store` | archival, fork modes, truncation parity — over SQLite |

**Gate 4:** each subsystem's parity tests pass; no-LLM compaction is gone.

### Phase 5 — Safety substrate  ·  P = MEDIUM–HIGH (plug into the seam)
Depends on Phase 2 orchestrator seam. 5.1–5.4 parallel (separate crates); 5.5 integrates last.

| WP | Owns | Parity target |
|---|---|---|
| 5.1 OS sandboxes + process-hardening | `browser-use-sandbox/src/{seatbelt,landlock,bwrap,windows,hardening}.rs` | per-platform enforcement parity |
| 5.2 execpolicy (Starlark) + amendments + command_canonicalization | `browser-use-sandbox/src/execpolicy/**` | declarative allowlist + persisted prefix amendments |
| 5.3 Network proxy/MITM + deferred network approval | `browser-use-sandbox/src/network/**` | per-host Allow/Deny/Ask, persisted rules |
| 5.4 Guardian (LLM reviewer) | `browser-use-guardian/**` | fail-closed, circuit breaker, ephemeral fork reviewer session |
| 5.5 Wire approvals into orchestrator; flip sandbox `None`→real; `PermissionRequest` hooks precedence | `core/src/orchestrator/**` | full approval→sandbox→escalate flow parity |

**Gate 5:** commands run sandboxed, approvals/guardian enforced, escalation works.

### Phase 6 — Advanced parity  ·  P = HIGH
| WP | Owns | Notes |
|---|---|---|
| 6.1 code_mode (V8 tools-as-code) | `browser-use-codemode/**` (new) | nested tool calls via the router |
| 6.2 agent_jobs (CSV fan-out) | `core/src/subagents/jobs.rs` | bounded concurrency, crash-recovery |
| 6.3 shared-service subthreads (codex_delegate equiv) | `core/src/subagents/delegate.rs` | approval forwarding to parent |
| 6.4 memories ingestion/storage | `browser-use-memories/**` (new) or `store` | beyond citation parsing |
| 6.5 connectors (OAuth MCP) + session prewarm | `browser-use-mcp/connectors.rs`, `core/src/session/prewarm.rs` | remote tool ecosystem |

---

## 4. Parallelism summary (waves)

- **Phase 0:** ~1 agent (serial carve) + 1 (harness). *Bottleneck — invest here.*
- **Phase 1:** 2 serial (schema, route) → **7-way parallel** wave.
- **Phase 2:** ~3-4 agents, coordinated (spine).
- **Phase 3:** **~10-way parallel** (one per tool).
- **Phase 4:** **~9-way parallel** (one per subsystem).
- **Phase 5:** ~4-way parallel + 1 integrator.
- **Phase 6:** ~5-way parallel.

Peak concurrency ≈ 9–10 agents (Phases 3/4) — but **parallelism is for isolation/depth, not speed**. Each agent works in its own worktree; the orchestrating agent merges at gates and runs the integration + parity + **e2e** tests. We optimize for *everything implemented, run, and tested*, not for finishing fast.

## 5. Cross-cutting tasks (not a phase)
- **De-codex rename:** done *inside each WP's owned files*; a final **audit WP** greps for residual `codex`/`CODEX_`/`oai`/`chatgpt` and confirms only intentional references remain.
- **Parity corpus:** grow the codex-parity fixture set continuously; every WP adds its own cases (own files).
- **End-to-end testing (required):** every phase gate **runs the real agent against a live model**, and a final full e2e pass exercises browser flows + tools + compaction + subagents + resume. Use the **existing codex auth** (already configured) as the live-model vehicle for dev/testing. *(Transition note: the codex backend provider is slated for removal — REARCHITECTURE §4 — so once OpenAI/Anthropic direct auth is wired, e2e should run through those; codex auth stays a fallback test vehicle until then.)*
- **Commit cadence:** tests committed first, then frequent working-increment commits per WP (Rule 7).
- **Decision log:** `DECISIONS.md` records sanctioned divergences + each "codex offered options, we picked X".

## 6. Decisions (settled) — see `DECISIONS.md`
1. **Context messages:** align with codex's typed `reference_context` mechanism for parity; our extra kinds become additional typed items.
2. **Subagent wait:** **event-notify** (in-memory); SQLite is write-only here.
3. **Crate granularity:** engine submodules + separate crates for `llm`/`mcp`/`sandbox`/`guardian`.
4. **Async migration:** top-down behind frozen Phase-0 interfaces, optimized for correctness/testability (not speed).
5. **v1 providers:** OpenAI (Responses), Anthropic (Messages + Claude-Code OAuth), Ollama, **DeepSeek, OpenRouter, Fireworks** (via the `openai-chat` protocol where compatible). More are just config entries — added later freely.

---

*This plan is intentionally static and review-ready. When we decide to execute, each WP becomes a sub-agent task seeded with its "Owns" set and the per-WP design-note template; phases run as waves with gates between them.*
