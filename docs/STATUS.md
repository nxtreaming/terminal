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
- SamplingDriver↔ToolDispatcher fusion is now WIRED (WP-I-fusion): the production `ModelSamplingDriver::with_fusion(dispatcher, recorder)` runs the model's tool calls through the `ToolDispatcher` (model order + parallel/serial gate) and records the assistant message + tool outputs via a `FusionRecorder` into the shared conversation, reporting `model_needs_follow_up` so the `TurnLoop` re-samples. The frozen `TurnLoop`/`SamplingDriver`/`SamplingOutcome` are unchanged; a text-only `ModelSamplingDriver::new` (no dispatcher) still exists. Remaining: production wiring of the `FusionRecorder` over the real `ContextManager`/`Session`-backed `TurnState` (the toolset/session integration WP).
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
- [x] T-tool_search — async BM25 deferred-tool ranking (codex bm25 2.3.2 SearchEngine, default limit 8, parallel_safe=true matches codex; registry/catalog wiring deferred) — 280 tests
- [x] T-web_search — hosted/provider-executed config+declaration (codex web_search.rs display helpers, mode Disabled/Enabled, passthrough run no real HTTP, parallel_safe=true) — 302 tests
- [x] T-browser — thin adapter over browser-use-browser (BrowserBackend trait + FakeBackend for tests; command/execute/observe/cancel; parallel_safe=false; sanctioned divergence, no codex analog) — 314 tests
- [x] T-python — thin adapter over browser-use-python-worker (PythonBackend trait + FakeBackend; run_with_timeout; parallel_safe=false; event-streaming/artifact-recording deferred) — 321 tests
- [x] T-mcp — async MCP tool-dispatch handler over seam (McpClient trait + FakeMcp; namespaced mcp__server__tool; CallToolResult mapping; transport+approval-gating deferred) — 332 tests

## ✅ TOOLS PHASE COMPLETE — decodex, 332 tests green, 10 handlers
shell, apply_patch, view_image(sync/serial), update_plan, request_user_input, tool_search, web_search, browser, python, mcp.
NEXT: INTEGRATION (real ToolRegistry/ToolSet dispatching all 10 by name + deferred catalog for tool_search; then wire ToolDispatcher→registry, SamplingDriver↔ToolDispatcher fusion, OrchestratorRunner real per-tool routing — un-stubs M3-core). THEN subsystems (compaction model-based, MCP transports, subagents, goals, skills/plugins, hooks+PermissionRequest, prompts de-brand, rollout). THEN safety (sandbox/execpolicy/network/guardian). FINAL cutover = [BLOCKED-NEEDS-HUMAN].

## INTEGRATION: ToolRegistry merged — decodex, 351 tests
- [x] I-registry — real ToolRegistry (DynTool trait-objects, dispatch-by-name, model_visible_definitions, deferred catalog) + ToolDispatcher routes through registry via orchestrator — 351 tests

### GAP (must fix): registry only dispatches 4/10 tools
ToolRegistry requires Req: DeserializeOwned. Deserialize-derived: update_plan, request_user_input, tool_search, web_search. NOT yet: shell, apply_patch, view_image, browser, python, mcp (derive only Clone/Debug/PartialEq). FOLLOW-UP WP (after fusion): add #[derive(Deserialize)] (+serde rename to camelCase wire names matching codex/legacy) to those 6 Request types + register them. browser/mcp Req are parsed/namespaced forms — may need a raw-args adapter. Until then those 6 tools are built+unit-tested but NOT reachable via the registry/dispatch path.
||||||| 5626f8a

## INTEGRATION: turn-loop ↔ sampling ↔ dispatch ↔ context fused — decodex-3-i-fusion
- [x] I-fusion — a turn now runs tools end-to-end. The production `ModelSamplingDriver`
  gained an optional fused path (`with_fusion(dispatcher, recorder)`, Option A per
  DESIGN.md): after streaming the model response it records the assistant message,
  dispatches the emitted tool calls through the `ToolDispatcher` (model order +
  parallel/serial gate), records each tool output via a new `FusionRecorder` seam into
  the SAME conversation the `TurnLoop` re-samples from, and reports
  `model_needs_follow_up=true` iff a tool ran; when the model emits no tool call the turn
  completes. Frozen `TurnLoop`/`SamplingDriver`/`SamplingOutcome`/`TurnState` unchanged;
  text-only `ModelSamplingDriver::new` retained. New `turn/fusion_tests.rs` proves the
  end-to-end loop (sample→dispatch→record→re-sample→complete), multi-call model-order
  recording, and the zero-tool one-iteration regression. (Crate-local agent tests green.)

## INTEGRATION: fusion merged — turn runs tools end-to-end
- [x] I-fusion — SamplingDriver fuses ToolDispatcher (Option A): sample→dispatch tool calls (model order, parallel/serial gate)→record outputs→re-sample; FusionRecorder seam; 347 tests. Remaining: a concrete TurnState+FusionRecorder over the live ContextManager/Session (session-integration WP).

## ✅ INTEGRATION COMPLETE — all 10 tools registered + dispatchable, decodex, 365 tests
- [x] I-derives — Deserialize on all 6 remaining Reqs (shell/apply_patch/view_image/python via direct derive; browser/mcp via WireArgs+From adapter); default_registry wires all 10 with parity ToolDefinitions + parallel_safe flags — 365 tests. The async engine is now functionally whole (engine + 10 tools + registry + e2e dispatch).
NEXT: SUBSYSTEMS (codex parity, one at a time): compaction(model-based) → MCP transports → subagents → goals → skills/plugins → hooks → prompts(de-brand) → rollout. THEN safety (sandbox/execpolicy/network/guardian). Cutover=[BLOCKED-NEEDS-HUMAN].

## SUBSYSTEMS phase (codex parity, one WP at a time)

### ✅ S-compaction — model-based history compaction — decodex @ 2ce916b, 373 tests
- [x] WP-S-compaction (`0bba56d`, merged `2ce916b`) — model-based compaction, NO no-LLM path (user-rejected). New `compact/{mod,tests}.rs` + `pub mod compact;`. `CompactionSampler` trait seam (model-only, no tool dispatch) drives ONE no-tools summary round-trip; `CompactingTurnState<S>` wires it into the frozen `TurnState::compact` hook (replaces ContextManager history → token accounting resets). Parity: `SUMMARIZATION_PROMPT` = codex `core/templates/compact/prompt.md` (compact.rs:46) verbatim; `SUMMARY_PREFIX` (compact.rs:47) byte-identical (399B) to legacy `COMPACTION_SUMMARY_PREFIX` (core constants.rs:26); summary msg = `format!("{SUMMARY_PREFIX}\n{suffix}")` (compact.rs:263); `COMPACT_USER_MESSAGE_MAX_TOKENS=20_000` (compact.rs:48); drop-oldest-on-ContextWindowExceeded retry (compact.rs:224-233); `build_compacted_history` = recent user msgs (≤20k via approx_token_count=len.div_ceil(4)) + prefixed summary. 8 new tests (compact::tests).
  - PARITY DEBT: CompactionSampler production impl (driving the real no-tools ModelClient stream) deferred to the ModelClient-wiring integration WP; token budget uses approx_token_count not the richer per-item assembly estimator (matches codex build_compacted_history_with_limit).

### ✅ S-mcp-transports — hand-rolled MCP transports — decodex @ 4e342e9, 389 tests
- [x] WP-S-mcp-transports (`f6c8bf3`, merged `4e342e9`) — stdio + streamable-HTTP(SSE) MCP transports + `McpConnectionManager` implementing the existing SYNC `McpClient` seam (tools/handlers/mcp.rs, unchanged). New `mcp/{config,http,manager,mod,oauth,protocol,stdio,tests}.rs` + `pub mod mcp;`. NOT vendoring `rmcp` (not in workspace) — hand-rolled JSON-RPC to the MCP wire spec. 16 new tests (389 total), network-free (loopback `127.0.0.1:0` TcpListener for HTTP/SSE + child-process script fixtures for stdio).
  - **Parity:** stdio framing = legacy `browser-use-core/src/mcp.rs` (spawn :732, init+`notifications/initialized` :776-804, newline JSON-RPC :1014-1040, monotonic i64 ids) but async background-reader + id→oneshot map; `mcp__server__tool` `__` delimiter = codex `codex-mcp/src/mcp/mod.rs:45-46,62-66` (matches seam `parse_namespaced`); parallel connect + per-server failure isolation = codex `connection_manager.rs:191,259-319` (JoinSet); `CallToolResult{content,isError,structuredContent,_meta}` passthrough = codex `:610-624` / legacy `:1059-1078`; config = codex `config/src/mcp_types.rs` + legacy `McpServerConfig`; OAuth PKCE S256 = codex `rmcp-client/perform_oauth_login.rs:11,19,404-405` (URL_SAFE_NO_PAD(sha256(verifier))) + `.credentials.json` token cache; elicitation → always `decline` (codex `elicitation_client_service.rs`).
  - **Sync-over-async bridge:** manager owns a dedicated multi-thread `tokio::Runtime` and `block_on`s inside the sync `McpClient::call_tool` (panic-safe from any caller context, not just spawn_blocking).
  - **Workspace-root Cargo.toml touched:** added `rand = "0.9"` to `[workspace.dependencies]` (PKCE CSPRNG); `sha2` already present. Crate adds reqwest(+stream)/tokio(+net)/rand/sha2 + Cargo.lock regen.
  - **PARITY DEBT:** interactive OAuth leg stubbed (`InteractiveNotWired`; PKCE/url/parse/cache real+tested; static `bearer_token` works e2e); OS-keyring store dropped (JSON file cache only); config simplified (dropped env_vars-process-source, bearer_token_env_var/env_http_headers indirection, required/supports_parallel_tool_calls/connector-id — per-tool parallelism via `McpToolInfo::read_only_hint`); stdio inherits child stderr (legacy buffers it); timeouts DEFAULT_STARTUP=10_000ms/DEFAULT_TOOL=60_000ms (legacy parity).

### ✅ S-subagents — roles + depth + event-notify mailbox — decodex @ 8f1c2d4, 413 tests
- [x] WP-S-subagents (`c312afe`, merged `8f1c2d4`) — roles-as-config-layer + spawn-depth limits + EVENT-NOTIFY mailbox (NOT poll), over the existing TaskDriver/EventSink/lifecycle seams with a FAKE ChildSpawner. NEW module `subagents/{mod,role,depth,mailbox,registry,spawn,manager,tests}.rs` + `pub mod subagents;`. 24 new tests (413 total), network-free.
  - **Parity:** spawn args = codex `multi_agents_v2/spawn.rs:242-289` (`SpawnAgentArgs` + deny_unknown_fields; `fork_turns` none/all/+int, default all, 0 rejected); tool name `spawn_agent` + required `[task_name,message]` = `multi_agents_spec.rs:75-109`; depth `DEFAULT_AGENT_MAX_DEPTH=1` (config/mod.rs:195) + `next_spawn_depth=saturating_add(1)` / `exceeds = depth>max` (agent/registry.rs:71-77); roles `AgentRoleConfig{description,config_file,nickname_candidates}` (config/mod.rs:1890-1898) + `apply_role_to_config` user-first-then-builtin, preserve provider/tier (agent/role.rs:38-83), built-ins default/explorer/worker; **EVENT-NOTIFY mailbox** = codex `session/input_queue.rs:25-80` (`watch::Sender<()>` + `Mutex<VecDeque>`; enqueue → `send_replace(())` wakes subscriber; FIFO drain; parent waits `rx.changed()` — NOT poll); `<subagent_notification>` (context/subagent_notification.rs:6-42); `<subagents>` block (codex agent/registry.rs:155-167 + legacy lib.rs:13400,13496-13498).
  - **ChildSpawner seam:** `async fn spawn_child(ChildSpec)->Result<ChildHandle>`; production (later WP) builds a child SessionTask (ModelClient+turn loop) → `TaskDriver::spawn_task`, child enqueues SubagentNotification on the shared Mailbox to wake parent `wait()`. Tests inject FakeSpawner + SilentSpawner (timeout path).
  - **PARITY DEBT:** AgentConfigLayer/RoleOverrides are module-local stubs (no on-disk TOML/ConfigLayerStack — resolution order/built-ins/override-wins/provider-tier-preservation match codex, the FS machinery doesn't); fork_turns parses but does NOT yet copy history into child (seam only); budget = single `child_usage_total` counter (deliberate add, not 1:1 codex multi_agent_usage_hint); nickname selection deterministic first-available (vs codex random choose); spawn_agent spec emitted as JSON not typed ToolSpec (integration WP maps it); AgentStatus is the needed subset (snake_case wire).

### ✅ S-goals — event-sourced goals + token budget — decodex @ 6fdc901, 434 tests
- [x] WP-S-goals (`fba5d04` → parity-fixed `46c34dc`, merged `6fdc901`) — event-sourced `GoalState` (`GoalEvent` Set/Updated/Accounted/Completed/Cleared + pure `reduce`/`replay`) + token-budget accounting + steering events. NEW module `goals/{mod,state,budget,steering,tests}.rs` + `pub mod goals;`. 21 new tests (434 total), network-free.
  - **Budget formula = FULL PARITY** `max(input - cached, 0) + max(output, 0)`, saturating: codex `core/src/goals.rs:1684-1688` (`non_cached_input().saturating_add(output.max(0))`) + legacy `browser-use-core/src/goals.rs:330-334` (`input - cached_input_tokens + max(output,0)`); reads `Usage.cached_input_tokens` (`browser-use-llm/src/schema/event.rs:21`, a subset of input). **PARITY FIX applied pre-merge:** the first commit charged FULL input (my WP prompt misread the user's `(input-cached)` spec as "include cached"); I caught it against the EXTREME-PARITY mandate, verified codex/legacy both subtract cached, and had the agent fix it (`46c34dc`) — I then re-read budget.rs myself to confirm. Tests `budget_subtracts_cached_input` (100/40/10→70) + `budget_clamps_non_cached_term_when_cached_exceeds_input`.
  - REUSES `context::accounting::approx_tokens_from_byte_count_i64` (shared `(b+3)/4`, no private copy — asserted by a test). Steering = `render_goal_context_message` (legacy `lib.rs:9796-9805` `{role:user, name:goal_context, input_text}`) + budget warn/exhaust crossing events (once per crossing) via injected `EventSink`. `GoalManager` ties state+budget+steering.
  - **PARITY DEBT:** soft warn-fraction `DEFAULT_WARN_FRACTION=0.8` is a local add (legacy has only the hard `>=` exhaust flip; hard boundary stays byte-for-byte); steering message *body* is a compact deterministic summary, not the full legacy `GOAL_CONTINUATION_PROMPT_TEMPLATE` (`constants.rs:48-98`) — later integration WP; `GoalManager` not yet wired into the turn loop (legacy call site `append_goal_progress_accounting` `goals.rs:201-248`) — later WP; `Usage` u64 widened to i64 at the boundary for the saturating/`max(0)` math.

### ✅ S-skills — discovery + budgeted injection + mentions — decodex @ c5e8a3f, 457 tests
- [x] WP-S-skills-plugins (`fdb1d6d`, merged `c5e8a3f`) — skill+plugin discovery (roots + precedence + dedup), ~2%-ctx budgeted `<skills_instructions>` injection, `$`/`@`/`skill://` mentions. NEW module `skills/{mod,discovery,inject,mention,tests}.rs` + `pub mod skills;`. 23 new tests (457 total), network-free (tempdir SKILL.md fixtures).
  - **Parity:** budget = codex `core-skills/src/render.rs:18` `SKILL_METADATA_CONTEXT_WINDOW_PERCENT=2` + `:17` `DEFAULT_SKILL_METADATA_CHAR_BUDGET=8_000` + `default_skill_metadata_budget` (`window*2/100` min 1, else 8000 chars) — replicated EXACT, verified by me reading inject.rs; discovery roots/precedence = legacy `available_skill_summaries` (`lib.rs:16848`: user/.agents/bundled/plugin/repo roots, sort `(scope_rank,name,path)`, dedup canonical SKILL.md first-seen `lib.rs:16974`) + codex `core-skills/src/loader.rs:213-221` scope_rank Repo<User<System<Admin; SKILL.md parser mirrors LEGACY hand-rolled `skill_frontmatter_value` (`lib.rs:17129`)/`skill_body_description_from_markdown` (`lib.rs:17119`); `<skills_instructions>` tag = codex `context/available_skills_instructions.rs` + legacy `render_available_skills_instructions` (`lib.rs:16281-16307`, tags `constants.rs:46-47`); mentions `$`=TOOL_MENTION_SIGIL / `@`=PLUGIN_TEXT_MENTION_SIGIL / `skill://` (codex `utils/plugins/src/mention_syntax.rs` + `core/src/plugins/mentions.rs`).
  - REUSES `context::accounting::approx_tokens_from_byte_count_i64` (asserted by test `budget_token_cost_uses_shared_div_ceil_4_heuristic`).
  - **PARITY DEBT:** `@` resolves skills too (superset of codex `@`=plugin-only); footer is shortened form of codex `SKILLS_HOW_TO_USE_WITH_ABSOLUTE_PATHS` (`render.rs:27`, $-trigger rule preserved); degrade-to-fit is single-pass (drop-desc-then-omit) vs codex 3-stage per-char description budget (`render.rs:205`); `SkillSource` enum vs codex `SkillScope` (config plumbing deferred); SKILL.md parser is legacy hand-rolled (no serde_yaml schema validation / openai.yaml sidecar); `skill://` tool-invocation (codex skills_handler) + skill *dependencies* (mcp_skill_dependencies) out of scope; SkillsManager not yet wired into turn-loop/context-assembly (later integration WP).

### ✅ S-hooks — hook runtime + matcher + PermissionRequest/Prompt/Agent — decodex @ 55d917f, 482 tests
- [x] WP-S-hooks (`872e4eb` → regex-matcher parity fix `90a52af`, merged `55d917f`) — hook runtime (matcher-group command hooks via `CommandRunner` seam) + `PermissionRequest` event + `Prompt`/`Agent` handler kinds. NEW module `hooks/{mod,event,config,runtime,tests}.rs` + `pub mod hooks;` + `regex.workspace=true` (crate opt-in; regex already a workspace dep). 25 new tests (482 total), network-free (FakeRunner + real ShellCommandRunner seam).
  - **Parity:** event kinds = legacy `HookEventName` (`browser-use-core/src/lib.rs:14031,14046-14054`: PreToolUse/PostToolUse/PreCompact/PostCompact/SessionStart/UserPromptSubmit/SubagentStart/SubagentStop/Stop) — implemented the 8 lifecycle kinds + added PermissionRequest/Prompt/Agent (sanctioned per user spec); matcher = **byte-for-byte legacy** `hook_matcher_matches` (`lib.rs:8354-8367`): None/empty/`*`→all, `hook_matcher_is_exact` (alnum/`_`/`|`, `:8369-8373`)→`|`-split literal eq, else raw unanchored `regex::Regex::new(m).is_match()` with `unwrap_or(false)` (invalid pattern matches NOTHING); deny-short-circuit = codex `run_pre_tool_use_hooks` (`hook_runtime.rs:185-217` `PreToolUseHookResult::Blocked`) + legacy `parse_hook_command_output` (`lib.rs:8588-8619`); timeout `DEFAULT_HOOK_TIMEOUT_SECS=60` (non-blocking on timeout). PermissionRequest emits `hook.permission_request` PendingEvent via injected EventSink (codex has the flow as a returned `PermissionRequestDecision` `hook_runtime.rs:222-253`, we additionally surface it as an event per user req).
  - **CommandRunner seam:** `async fn run(command,stdin_json,timeout)->CommandOutput`; production `ShellCommandRunner` (real `/bin/sh -c`, stdin JSON, tokio timeout) is REAL not stub; tests inject FakeRunner. Decision = parse stdout JSON `HookDecision`, else exit-code-2 ⇒ block w/ stderr reason (Claude-Code/codex convention).
  - **PARITY DEBT:** codex `hooks.rs`/`hooks_config.rs`/`hook_runtime_tests.rs` ABSENT in this codex checkout (only `hook_runtime.rs`, a facade over sibling `codex_hooks` crate) → legacy `browser-use-core` was authoritative (per WP guidance); `PreCompact`/`PostCompact` kinds omitted (separate compaction concern); matcher follows legacy's RAW-regex (not `^(?:pat)$` anchored) deliberately; not yet wired into turn-loop/tool-dispatch/approval (PreToolUse→dispatch, UserPromptSubmit/Prompt→prompt boundary, Stop/Agent→task lifecycle, SubagentStart/Stop→subagents, PermissionRequest→tools/approval) — later integration WP.

### ✅ S-prompts — agent-crate prompt module (de-branded) — decodex @ 7660544, 490 tests
- [x] WP-S-prompts (`821b903`, merged `7660544`) — NEW `prompts/{mod,tests}.rs` + `pub mod prompts;`. The agent crate had NO prompt surface; this gives it the browser-use system prompt + tool descriptions + collaboration modes + compacted-context + helper-session + review, via `include_str!("../../../../prompts/<file>.md")` (4-level path, verified compiles). 8 new tests (490 total).
  - **De-brand:** the model-facing assets were ALREADY browser-use branded (`browser-agent-system.md` 0 codex hits — "You are a browser-use agent built around the bitter lesson…"). Guard test `model_facing_prompts_have_no_codex_or_chatgpt_brand` asserts no codex/chatgpt in all 10 consts. KEPT preamble + interaction-skills (`interaction-skills/connection.md` → `BROWSER_CONNECTION_GUIDANCE`) + screenshot/view-image notes (asserted). The `codex-model-fallback-prompt.md`/`codex-models.json` are the intentional codex-model fallback path (legitimately contain "Codex") — out of scope, not guarded. NO `/// Codex parity:` comments altered (verified by me — diff has zero parity-comment lines).
  - **Approach:** evaluated re-export over `browser-use-providers` (rejected: providers exposes only fully-assembled `String` builders coupled to ModelPersonality/ModelCatalog + the codex-fallback path; the granular branded consts are private there / absent). Ported via `include_str!` of the SAME repo-root `prompts/` assets (single source of truth — same pattern as core/browser/providers; NOT a content fork). Mirrors legacy `crates/browser-use-core/src/prompts.rs` accessors (`collaboration_mode_instructions`→`collaboration_mode_prompt(CollaborationModeKind)`, `compacted_context_system_message`, `render_prompt_template`).
  - **PARITY DEBT:** asset `.md` now referenced from 5 crates (content shared, NOT forked); the prompt ASSEMBLY logic (preamble+personality+interaction-skills loop+codex-fallback) lives in providers' `browser_agent_instructions_*` and is NOT reused — at cutover, agent engine should depend on providers' assembly OR providers expose granular consts (one assembly path); module not yet wired into request base-instructions (cutover/request-building WP); `compacted_context_system_message` returns `serde_json::Error` not `anyhow::Result` (avoids widening deps, behavior identical).

### ✅ S-rollout — truncation + by-turn fork + archival — decodex @ f71db86, 505 tests
- [x] WP-S-rollout (`b52e7e3`, merged `f71db86`) — 5MiB-bounded rollout truncation + by-TURN fork + SQLite-write-sink archival. NEW `rollout/{mod,truncation,fork,archive,tests}.rs` + `pub mod rollout;`. 15 new tests (505 total), network-free (real Store on tempdir).
  - **Parity (CORRECTED by agent):** the brief's `DEFAULT_THREAD_ROLLOUT_MAX_BYTES`/`truncate_rollout_if_needed` DON'T EXIST; codex `thread_rollout_truncation.rs` is purely turn-boundary. Real byte-truncation = codex `core/src/conversation_history.rs:11` `MAX_ROLLOUT_BYTES_BEFORE_TRUNCATION = 5*1024*1024` + `truncate_if_needed:44-76` (drop-oldest-until-fit, always keep ≥ most-recent, byte sizing `serde_json::to_string(item).len()`) — replicated EXACT (I verified the codex source + the agent's const myself). By-turn fork = legacy `rollback.rs::rollback_last_n_user_turns:73` + `is_real_user_event:114` (reused agent `session::is_real_user_event`, not duplicated). Archival = legacy `persistence.rs` append discipline + codex `rollout-trace/{bundle,writer}`.
  - **Both logged debts FIXED:** `ForkMode::LastN` now by-TURN (test `fork_lastn_is_by_turn_not_by_message`: LastN(2)/4-turns → seqs [5,6,7,8] ≠ naive [7,8]); `Summary ≠ All` (collapses all-but-last-turn into `SummaryPlaceholder{collapsed,summary}`, `ForkOutcome.summary` Some for Summary / None for All).
  - **!Sync-safe:** `StoreRolloutArchiver` holds `Arc<Mutex<Store>>`, every SQLite touch in `spawn_blocking` (same as session/sink.rs).
  - **PARITY DEBT:** Summary placeholder has no real summarizer text yet (legacy/codex have none either — explicit seam, not aliased to All); archiver dumps `rollout.archived` event rows (write-only, excluded from replay) rather than a separate JSONL bundle dir (codex rollout-trace) — future refinement; RolloutManager not wired into engine (at cutover: call truncate_and_archive post-turn + switch Session::fork to fork_events_by_turn).

---

## ✅ ALL 8 SUBSYSTEMS COMPLETE — decodex @ f71db86, 505 tests green, workspace builds
compaction ✅ mcp-transports ✅ subagents ✅ goals/budget ✅ skills/plugins ✅ hooks ✅ prompts ✅ rollout ✅. Each codex/legacy-parity, network-free tested, verified-in-worktree before merge. Two wrong-premise agent simplifications caught + fixed pre-merge (goals cached-subtraction; hooks regex-availability); one parity-source correction accepted (rollout byte-truncation lives in conversation_history.rs not thread_rollout_truncation.rs).

## ▶ SAFETY phase (4 WPs, one at a time) — STARTING
- [ ] **WP-Safety-1 sandbox-backends** (IN PROGRESS) — real OS sandbox backends behind the existing `tools/sandbox.rs` seam (today everything resolves to `SandboxType::None`). seatbelt(macOS)/landlock+seccomp(linux)/bwrap + process-hardening. Codex `core/src/tools/sandboxing.rs` + seatbelt/landlock sibling crates. NEW module (prefer `sandbox_backends/*`), network-free tests (NO real sandbox escape), platform-gated parts flagged as parity debt.
- [ ] WP-Safety-2 execpolicy — Starlark policy + amendments + command_canonicalization (codex `exec_policy.rs` + execpolicy crate).
- [ ] WP-Safety-3 network-proxy — proxy/MITM + deferred network approval (codex `network_proxy_loader.rs`).
- [ ] WP-Safety-4 guardian + approval-wiring — LLM-reviewer (fail-closed, circuit breaker) + flip orchestrator sandbox None→real + PermissionRequest precedence (consumes hooks PermissionRequest event). Touches existing orchestrator wiring — scope carefully.

## FINAL CUTOVER = [BLOCKED-NEEDS-HUMAN]
After all 4 safety WPs: switch browser-use-tui + browser-use-cli from browser-use-core → browser-use-agent, retire browser-use-core. ONLY human-gated step — do NOT do autonomously.
