# STATUS — verified state & how we continue (replan after the swarm incident)

_Last updated: 2026-05-30. Single source of truth for what is DONE vs NOT._

## Trust rule
Only commits **made and test-verified in the working session** are trusted. A separate
agent ran on similarly-named branches (`sanfrancisco-*`) and the branch namespace got
tangled. **Do not merge, rebase, or delete anything under `sanfrancisco-*` or any ref whose
provenance is unconfirmed.** Verify with `cargo test` (with a `timeout`) before trusting.

## What got fucked (post-mortem)
1. **Parallel git mutations raced** (cherry-pick + worktree-remove at once) → orphaned commits onto stray branches.
2. **A real bug × no isolation:** `scrub()` infinite loop hung ~16 test binaries; every parallel agent re-ran the full suite → load avg 21, "stuck terminals". **Fixed in `8481788`.**
3. **Worktrees branched from wrong bases / random names** (`sanfrancisco-*`), partly from a different agent.
4. **Wrong framing:** "loop until the whole rewrite is done" swarmed a mostly-**serial**, months-scale job. That was the core mistake.

## DONE & verified (on `decodex`, this session)
- **`browser-use-llm` crate — green, 74 tests** (`cargo test -p browser-use-llm`, runs in <1s):
  - `schema/` typed canonical model (request/message/content-parts/events+lifecycle/usage/options/errors)
  - `route/` traits (Protocol/ProtocolStream/Endpoint/Auth) + **async ModelClient/executor** (retry/backoff, rate-limit parse, secret redaction)
  - `protocols/` 3 wire protocols: OpenAI Responses, OpenAI Chat (ollama/openrouter/deepseek/fireworks), Anthropic Messages — each with fixture decode tests
  - `protocols/utils/` Lifecycle + ToolStream + SSE framing
  - **scrub() infinite-loop bug fixed** (`8481788`)
- **Phase 0.1 carve (partial):** ~16 cohesive modules extracted from `browser-use-core/src/lib.rs` (47.3k→44.3k lines); core suite was 488 green at extraction. Plateaued at the tangled core (needs the async rework).

## RECOVERABLE but NOT yet integrated (mine; behind backup refs)
- `backup/wp17-facades-43187b2` — provider facades (OpenAi/Anthropic/OpenAiCompatible) + its own scrub fix. **Conflicts with our scrub fix** — take only the `providers/` files.
- `backup/wp18-toolruntime-3c9b850` — define-once `Tool` + `tool_runtime` loop (committed on stray branch `sanfrancisco-llm-toolrt`).
- `backup/decodex-pre-reset-5352056` — the full pre-fix tip.
- `/tmp/backup-rearchitecture-notes/` — parity design notes (useful research from the agent fan-out).

## DO NOT TOUCH
- `sanfrancisco-ledger-validation`, `sanfrancisco-llm-toolrt` (branches + worktrees) — other agent / orphaned.
- `origin/decodex` is pinned at `046a80a` (planning checkpoint). Local `decodex` is ahead with verified work; **push only when a milestone is fully green and you say so.**

## How we continue (milestone-based, NO swarm)
**Hard process rules now:**
- One WP at a time. Sequential git ops only — never parallel mutations.
- Every test run wrapped in `timeout` (e.g. `timeout 300 cargo test ...`) so a hang can't pin the machine.
- Worktrees (only when files are genuinely disjoint) named `decodex-<feature>` from the current tip; removed after merge.
- Fix bugs before any fan-out. Verify before trusting any pre-existing commit.

**Milestone 1 — finish & land the LLM crate (low-risk, next):**
1. [x] Restore verified baseline + fix scrub() (`8481788`)
2. [x] Integrate WP 1.7 facades (`providers/` files only; 89 crate tests green; `7f63ef2`)
3. [x] Integrate WP 1.8 tool_runtime (`tool.rs` + `tool_runtime.rs`; 102 crate tests green; `2ef9e29`)
4. [x] `timeout 300 cargo test -p browser-use-llm` green → **MILESTONE 1 DONE** (102 tests, <1s, no hang)

**Milestone 2 — finish the carve** — ✅ **DONE at its working boundary.**
- ~16 cohesive modules extracted; `lib.rs` 47.3k → 44.3k; `browser-use-core` 488 tests green at `ecfd2bf`.
- A further pass found ~4 more async-free extractable clusters (`response_items`, `model_catalog_remote`,
  `task_analytics`, `image_artifacts`) BUT could not land them due to **branch contention** (two
  committers on `decodex` raced — the loop driver + the carve sub-agent), so it reset to the green
  baseline `ecfd2bf`. **Root-cause fix going forward:** never run a sub-agent that commits to `decodex`
  while the loop also commits to `decodex`. Sub-agents commit on their OWN branch/worktree only.
- Those 4 clusters are deferred — they'll fall out naturally during the **B: parallel rewrite** below,
  so we are NOT going to keep poking the monolith on `decodex`.
- The async-bound residue (context, compact, session, events, agents_md, hooks, skills, plugins,
  subagents, the turn loop, providers_glue) is M3 work.

**Milestone 3 — the async engine — STRATEGY CHOSEN: (B) PARALLEL REWRITE.**
Build a NEW async engine crate (`browser-use-agent`, working name) alongside the existing sync
`browser-use-core`, on top of the frozen async `browser-use-llm`. Port subsystem-by-subsystem with
codex-parity tests; cut over (TUI/CLI switch from core → agent) only at the end. **Why B fits now:** a
new crate = brand-new files = ZERO contention with the monolith — which is exactly the failure mode that
kept biting us. Sub-agents can even work disjoint *new* files safely.
M3 sub-plan (each its own WP, verified+committed before the next; the FIRST few are isolated new files,
so they're loop-safe; later cutover steps are human-gated):
- 3.1 scaffold `browser-use-agent` crate (deps: tokio, browser-use-llm, browser-use-protocol, browser-use-store) — compiles empty, 0 tests.
- 3.2 async turn loop (codex parity: unbounded loop on needs_follow_up, CancellationToken, FuturesOrdered) — pure logic + scripted-source tests, no real I/O.
- 3.3 context manager with REAL token accounting (per-provider `Usage` from browser-use-llm).
- 3.4 ToolOrchestrator/ToolRuntime/Approvable/Sandboxable seam (sandbox=None stub) — additive new files.
- 3.5 session/resume over SQLite as a write-sink + event-notify.
- 3.6+ port tools, then subsystems, then safety (Phases 3–6 of IMPLEMENTATION_PLAN), each on the new crate.
- FINAL (human-gated): switch TUI/CLI to the new engine; retire `browser-use-core`.

See `IMPLEMENTATION_PLAN.md` for the full WP list and `REARCHITECTURE.md` for the design.

---

## M3 progress log (browser-use-agent, on `decodex`)

Build mechanics that WORK: one sub-agent per WP, each in its own `decodex-3-<wp>` worktree forked
from the current `decodex` tip, committing ONLY to its own branch. Main loop merges one at a time
(`--no-ff`, disjoint files → clean), runs `timeout cargo test -p browser-use-agent` green, then
removes the worktree+branch. NEVER two committers on `decodex`. NEVER batch git mutations in one
parallel block (that caused every mishap — wrong-SHA worktrees, deleted branches). Verify SHAs with
`git rev-parse` before use. Detect dead agents (branch HEAD unmoved + file still `unimplemented!()`)
and relaunch; recover orphaned commits via `git merge <dangling-sha>` if a branch ref was lost.

**Wave 0:** [x] WP-A0 frozen scaffold (`fc36a73`, 38 files)
**Wave 1 (pure decision cores) — COMPLETE, 134 tests green @ `2a79f5d`:**
- [x] A1 loop_decision+retry (`b8f0e99`, 20t)  · [x] A2 tool seam+decisions (`b40af45`, 36t)
- [x] A3 context accounting/assembly (`119e856`, 36t) · [x] A5 events mapper (`037ceb3`, 16t)
- [x] A6 session reducer+rollback (`6466fcb`, recovered from orphan, 6t) · [x] A4 context inject (`6edf672`, 21t)

**PARITY DEBT to reconcile before cutover (logged, not blocking):**
- **A4 vs A6 duplication:** `context/inject.rs` (A4) emits *generic field-agnostic Value-diff* context
  messages; `session/reconstruct.rs` (A6) has the *legacy-faithful* builders (`workspace_context_message`,
  `permissions_context_message`, `model_switch_context_message`, `move_workspace_context_before_first_user_message`).
  Two impls of the same concept; A4's shapes are NOT byte-identical to legacy. RESOLUTION: when the
  ContextManager async wrapper (B2) is built, make `inject` reuse the reconstruct builders (single source
  of truth) and drop A4's divergent shapes, OR promote the legacy builders to `context::inject` and have
  reconstruct call them. Pick one; add a golden test that inject output == reconstruct builder output.
- A1 `backoff_ms` deterministic (codex jitters — apply jitter in B5 sampling I/O layer).
- A3 `non_last_reasoning` filter approximated on Value currency; A3 `strip_images` removes vs codex placeholder text.
- A6: `default_permissions_instructions` placeholder; helper-session prompt text inline (not byte-identical
  to the `.md` templates); image-replay disk-IO branch omitted from the pure reducer (belongs in async layer).

**Wave 2 (async wrappers — each delegates to a merged Wave-1 pure core; own worktree each):** STARTING
- B1 tools/orchestrator.rs (needs A2) · B2 context/mod.rs async wrapper (needs A3+A4; resolve the debt above)
- B3 events/store_sink.rs (needs A5) · B4 session/{mod,sink,resume,notifier}.rs (needs A6)
- B5 turn/sampling.rs (needs A1+A5)
**Wave 3 (integration):** C1 turn/dispatch.rs · C2 turn/{loop_driver,mod}.rs · C3 task/{driver,abort,lifecycle}.rs

---

## M3 Wave 2 COMPLETE — decodex @ 8f3c9f2, 172 tests green, workspace builds

- [x] B1 ToolOrchestrator (async, delegates to pure plan_attempts) — `acbf3b9`
- [x] B2 ContextManager async wrapper + RESOLVED A4/A6 inject parity debt (inject now byte-identical to legacy builders) — `44ce457`
- [x] B3 StoreSink (dedicated-OS-thread writer; rusqlite Connection is !Sync so NOT spawn_blocking) — `3ac6640`
- [x] B4 Session lifecycle create/resume/fork/rollback + event-notify (not poll) over SQLite — `ab23a44`
- [x] B5 SamplingDriver (stream+retry over pure decision; network-free via ScriptedTransport; I/O-layer jitter) — `9c33c5e`

LESSONS (cost real time, now hard rules):
- An agent CAN falsely report "green" (B5's first commit `ab11b76` didn't even compile). ALWAYS re-run `cargo test` myself in the agent's own worktree before merging, and again on decodex after merge.
- `browser_use_store::Store` (rusqlite Connection) is `!Sync` — any SQLite-touching async WP must use a dedicated OS thread + channel, NOT Arc<Store> across tokio tasks.
- Garbled shell-output buffer caused me to run merges repeatedly + a stray reset; recovered via reflog (no work lost). Use `git -C <abs>` everywhere; never rely on cwd; one git op per command.

Wave-2 parity debt still open (revisit pre-cutover): B5 transport-switch is a no-op (WS fallback not wired in browser-use-llm); ForkMode::LastN truncates by message not turn-boundary; ForkMode::Summary==All; session/reconstruct.rs still has private copies of context-message builders (inject is now SoT, mechanical follow-up to dedupe).

## Wave 3 (integration spine — SERIAL, each needs the prior): STARTING
- C1 turn/dispatch.rs — FuturesOrdered ordered tool dispatch + RwLock parallel/serial gate (needs B1)
- C2 turn/{loop_driver,mod}.rs — the unbounded turn loop wiring decision::classify_loop_step + sampling + dispatch + context (needs A1+B5+C1+B2)
- C3 task/{driver,abort,lifecycle}.rs — task driver, spawn/abort, graceful-100ms-then-hard, lifecycle events (needs C2+B4+B3)

---

## ✅ M3 CORE ENGINE COMPLETE — decodex @ 8d40ff0, 194 tests green, full workspace builds

Wave 3 (integration spine) merged:
- [x] C1 turn/dispatch.rs — FuturesOrdered ordered dispatch + RwLock parallel/serial gate (`fc45445`)
- [x] C2 turn/{loop_driver,mod}.rs — unbounded decision-driven turn loop, NO max-turns (`41ce0c9`)
- [x] C3 task/{driver,abort,lifecycle}.rs — one-active-task spawn/replace, graceful-100ms-then-hard abort, TurnStarted/Complete/Aborted lifecycle, InterruptedTurnHistoryMarker (`ae6e291`)

The new async `browser-use-agent` crate now has the full engine skeleton wired:
schema/route/protocols/providers/tool-runtime (Wave 1 llm + cores) → orchestrator/context/store-sink/session/sampling (Wave 2) → dispatch/turn-loop/task-driver (Wave 3). 194 unit/integration tests, all pure/scripted (no network), green.

### What is FUNCTIONAL vs STUBBED at M3-core-complete (honest)
The control-flow + decision logic is codex-faithful and tested, but several bodies are intentionally stubbed pending their own WPs:
- Compaction body: control flow (compact-then-continue) is parity-correct; the model summarizer is a TurnState::compact() hook (no-op default) — real WP pending.
- SamplingDriver↔ToolDispatcher are SEPARATE seams; a production SamplingDriver must fuse dispatch (run tool calls, record outputs) — wiring pending the toolset WP.
- OrchestratorRunner is a placeholder that records tool-result Messages rather than routing real per-tool Req/Out through ToolOrchestrator::run — pending toolset WP.
- No real tools yet (shell/apply_patch/browser/etc.), no real model HTTP call exercised end-to-end, no sandbox/guardian/network.
- Logging: crate has no tracing facade; hard-abort/store-error paths surface via events/return, not logs.

### NEXT (autonomous, one WP at a time, isolated worktrees, verify-before-merge):
TOOLS PORT → SUBSYSTEMS (compaction model-based, MCP transports, subagents, goals, skills/plugins, hooks+PermissionRequest, prompts de-brand, rollout) → SAFETY (sandbox/execpolicy/network/guardian).
FINAL CUTOVER (TUI/CLI: browser-use-core → browser-use-agent; retire core) = [BLOCKED-NEEDS-HUMAN] — do NOT do autonomously.

## Tools port (on decodex, M3 core complete @ a21047b base)
- [x] T-shell — async shell/exec tool over ToolRuntime seam (tokio::process, 10s default timeout/exit124, 1MiB cap, simple rm-rf denylist; full tree-sitter denylist TODO) — 212 tests
- [x] T-apply_patch — async V4A apply_patch over ToolRuntime seam (envelope+hunks, path-safety reject-outside-root; fuzzy-match/turn_diff_tracker/metadata-denylist deferred) — 231 tests
- [x] T-view_image — sync/serial (INTENTIONAL divergence, parallel_safe=false), blocking std::fs::read, base64 data-url in ExecOutput.stdout (resize/structured-content deferred) — 244 tests
- [x] T-update_plan — async update_plan over seam (codex StepStatus pending/in_progress/completed wire vals, one-in_progress validation, parallel_safe=false matches codex) — 258 tests
- [x] T-request_user_input — async (request side; host round-trip deferred), codex questions/options wire shape + normalize validation, parallel_safe=false — 269 tests
