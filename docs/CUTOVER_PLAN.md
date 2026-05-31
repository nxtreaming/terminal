# CUTOVER PLAN — wire `browser-use-agent` into tui+cli, then delete `browser-use-core`

_User green-lit: "wire first, then delete." Work happens in worktree `terminal-decodex-cutover` (branch `decodex-cutover`, off `decodex` @ 24970ba)._

## Honest size: HUGE (not a quick carve). Deletion is the last trivial step.

### Verified ground truth
- The new engine `browser-use-agent` (~40k LOC) exists only in the `decodex` worktree. tui/cli currently depend on `browser-use-core` (~62k LOC) and have **NO dep edge to the agent crate**.
- Agent is already de-cored (0 `browser_use_core` symbols; only shares `browser_use_protocol::{EventRecord,ModelUsage}`). The "de-core the agent" phase is a no-op — already done.
- The engine has only ever run on **scripted test doubles**. `ModelClientTransport` (real HTTP) is typed but never instantiated; no live e2e. 601 tests green, all against fakes.

### The hard blockers (real new code, not glue)
1. **No app-facing facade** on `browser-use-agent` — no build-from-config `Engine`/`AgentSession`, no submit-input, no event-stream, no store-bound session ctor. tui/cli need a ~60-symbol contract; none exists.
2. **Store binding not built** (WP-B4-prod): production `EventSink`/`EventSource` over `browser-use-store`. Spine of resume/rollback/agent-tree.
3. **No production `TurnState`** (WP-B2-prod) over `ContextManager`+`Session`; no runtime path constructing `ModelClientTransport`.
4. **~60-symbol core API surface** wider than "run a turn": agent-tree ops, status, review-prompt builders, message-history, `product_analytics`, crypto-provider install, exec cleanup, 3 `record_*` hooks. Each is a parity surface the facade must reproduce/port.
5. **Deferred seams still stubs**: compaction/guardian-reviewer/child-spawner/MCP-OAuth (trait seams, no prod impl); hooks/goals/skills/rollout/prompts built but not called from the turn loop; secured orchestrator not the default. Compile-OK but runtime behavioral gaps vs core.
6. **Parity unproven at runtime** — rewrite verified vs codex by reading, not differential testing. Gaps will surface at real runtime.

### What's autonomously reachable vs human-gated
- **Autonomous:** facade + store binding + prod TurnState + seam wiring + repoint tui/cli → **compiles + runs on a fake/scripted provider + 601 tests green + fake-backend smoke**.
- **Human-gated:** the first **live-model** turn (needs real API keys/auth; auth currently flows through legacy `browser_use_providers`, not `browser-use-llm`). Differential parity vs core also needs a human in the loop.
- **Trivial, last:** delete `browser-use-core` (rm dir + drop workspace member + dep lines) — ONLY after tui/cli build+run on the new engine. Irreversible except by git revert.

## Ordered phases (each keeps the workspace COMPILING; core deleted last)
- **A. Baseline** ✅ — workspace green at decodex HEAD; agent already de-cored (dead dep line dropped, committed).
- **B. Store binding** (WP-B4-prod) — real `EventSink`/`EventSource` over `browser-use-store`; store-bound `Session` ctor + `ContextManager` persist. **← STARTING HERE.**
- **C. Production `TurnState` + real `ModelClient` path** (WP-B2-prod) — `TurnState` over ContextManager+Session; construct `ModelClientTransport`; provider+auth config.
- **D. App-facing facade** — `Engine`/`AgentSession`: build-from-config, submit-input, event-stream, persist/resume + the ~60-symbol compat surface (port agent-tree/status/review/history/analytics).
- **E. Wire deferred seams** into the turn loop — hooks/goals/skills/rollout/prompts; secured-orchestrator flip; prod impls for compaction/guardian/childspawner/MCP-OAuth.
- **F. Repoint tui then cli** at the facade; rewrite call sites (tui main/runtime/settings/render/transcript; cli main). `cargo build --workspace` green after each.
- **G. Verify** — `cargo build --workspace` + `cargo test --workspace` green; fake-backend smoke run. **[HUMAN: live-model e2e here.]**
- **H. Delete `browser-use-core`** — rm crate, drop workspace member + dep edges; workspace green.
- **I. De-codex audit** — grep residual brand outside intentional fallback refs.

## Status log
- A done (baseline green, agent dead-dep dropped).
