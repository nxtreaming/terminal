# HANDOFF — autonomous M3 rearchitecture driver (read this first)

_Written at context-compaction. You are continuing an AUTONOMOUS loop. The user is ASLEEP and said: "make sure to complete the entire plan — DO NOT STOP or ask me anything." Keep going via ScheduleWakeup; do not wait for the user._

## The mission
Rebuild the terminal's agent engine as a NEW async crate `browser-use-agent` (strategy B: parallel rewrite) alongside the legacy sync `browser-use-core`, on top of the already-built async `browser-use-llm`. Port subsystem-by-subsystem to **codex parity**, with real tests, then (human-gated) cut the TUI/CLI over and retire core. Work happens on branch **`decodex`** in worktree **/home/exedev/new-core/terminal-decodex**.

## Authoritative docs in the repo (READ THESE)
- `docs/STATUS.md` — the live ledger: what's done, lessons, parity debt, next phases. **Update it after every WP.**
- `docs/agent-design/DESIGN.md` + `frozen-interfaces-sketch.rs.txt` — the frozen interface contract for the agent crate.
- `IMPLEMENTATION_PLAN.md`, `REARCHITECTURE.md`, `DECISIONS.md` — overall plan/decisions.
- `docs/CARVE_PLAN.md` — the (paused) lib.rs carve.

## CURRENT STATE (as of handoff)
- **decodex @ 6fedffd** = M3 CORE ENGINE COMPLETE + shell tool. 212 agent tests green, full workspace builds.
- **M3 core engine DONE & merged**: Wave 1 (A1-A6 pure decision cores), Wave 2 (B1 orchestrator, B2 context-mgr, B3 store-sink, B4 session, B5 sampling), Wave 3 (C1 dispatch, C2 unbounded turn loop, C3 task driver). All in `crates/browser-use-agent/src/`.
- **Tools-port phase IN PROGRESS**: T-shell ✅ merged (212 tests). **T-apply_patch JUST FINISHED in worktree `decodex-3-t-applypatch` (commit `83fb938`, claims 231 tests) — I was mid-verification when context ran out.** FIRST ACTION: read `/tmp/apv.txt` (the verification I just ran); if it shows `test_exit=0 errors=0` and owns only apply_patch.rs/apply_patch_tests.rs/handlers/mod.rs, then MERGE it (protocol below), else investigate/relaunch.

## NEXT WORK (one WP at a time, serial — tools all touch handlers/mod.rs)
TOOLS (each = NEW disjoint files under `crates/browser-use-agent/src/tools/handlers/`, mirror `shell.rs` as the template for the ToolRuntime seam, codex parity from `/home/exedev/repos/codex/codex-rs/core/src/tools/handlers/<tool>.rs` + legacy `/home/exedev/new-core/terminal-decodex/crates/browser-use-core/src/{tools/*,lib.rs}`, real tests, drive ≥1 test through `ToolOrchestrator::new(NoneSandboxProvider, AutoApprover).run(...)`):
apply_patch (finishing) → **view_image (KEEP sync/serial — intentional divergence, parallel_safe=false, blocking base64 read)** → update_plan → request_user_input → tool_search → web_search → browser adapter (thin, over browser-use-browser) → python adapter (over browser-use-python-worker) → mcp dispatch.
SUBSYSTEMS: compaction (**MODEL-BASED, NO no-LLM path** — fill the `TurnState::compact` stub + real summarizer via a no-tools sampling pass), MCP transports (HTTP/SSE/OAuth/elicitation), subagents (roles/depth/**event-notify not poll**), goals/budget, skills/plugins, hooks (+PermissionRequest event + Prompt/Agent handlers), prompts (de-brand), rollout hardening.
SAFETY: OS sandbox backends (seatbelt/landlock+seccomp/bwrap/windows), execpolicy, network proxy, guardian + approval wiring.
ALSO wire the M3-core STUBS once enough tools exist (see STATUS.md "functional-vs-stubbed"): SamplingDriver↔ToolDispatcher fusion; OrchestratorRunner real per-tool routing; the compaction body.
**FINAL CUTOVER (TUI/CLI core→agent, retire core) = [BLOCKED-NEEDS-HUMAN]. Do NOT do it autonomously** — mark it in STATUS.md, keep the loop alive.

## THE MERGE PROTOCOL (follow exactly — this is hard-won)
1. Agent reports done → in ITS worktree run `timeout 250 cargo test -p browser-use-agent` **MYSELF**. Agents HAVE FALSELY reported green (one committed non-compiling code). NEVER trust the report. Confirm `git diff --name-only <decodex-tip>..HEAD` shows ONLY that WP's owned files; `git rm` any stray `.bak`/`.orig`.
2. If green: `git -C /home/exedev/new-core/terminal-decodex merge --no-ff --no-edit <branch>`. Resolve Cargo.toml conflicts: tempfile → `tempfile.workspace = true`; tokio feature lists → UNION the features; handlers/mod.rs `pub mod` lines → keep ALL. Complete commit. Then `timeout 280 cargo test --manifest-path .../terminal-decodex/Cargo.toml -p browser-use-agent` MUST be green AND `timeout 400 cargo build --manifest-path .../terminal-decodex/Cargo.toml --workspace` MUST be green.
3. If a merge breaks decodex: `git -C .../terminal-decodex reset --hard <pre-merge-sha>` and relaunch the WP fresh.
4. `git worktree remove --force <dir>` + `git branch -d <branch>`; tick `docs/STATUS.md`; commit STATUS; launch NEXT WP in its own worktree forked from CURRENT decodex tip (`TIP=$(git -C ... rev-parse HEAD)`).
5. Dead agent (branch unmoved + files still unimplemented!()/absent + output file stale ~30min) → remove worktree+branch, relaunch ONE fresh agent. Recover orphaned commits via `git merge <dangling-sha>` if a branch ref was lost (check reflog).

## HARD RULES (every one was learned from a failure this session)
- **Sequential git ONLY. NEVER batch git mutations in one parallel tool-call block** — that caused wrong-SHA worktrees, deleted branches, repeated merges. ONE git op per Bash call.
- Use `git -C <abs-path>` always; never rely on cwd (the shell cwd resets to terminal-improve-benchmarks each call).
- **NEVER `pkill -f browser_use`** — the pattern matches the pkill command's own line and it suicides the shell (exit 1, aborts the batch). To clear stray test binaries: let them exit, or `kill -9 <pid>` by explicit PID. Check load with `uptime` instead.
- Verify every SHA with `git rev-parse` before using it in a worktree-add. Capture into a var: `TIP=$(git -C ... rev-parse HEAD)`.
- Wrap EVERY cargo invocation in `timeout` (a `scrub()` infinite-loop bug once hung 16 test binaries → load 21). That bug is FIXED (`8481788`).
- Worktree/branch naming: `decodex-3-<wp>` (e.g. `decodex-3-t-view-image`). Prefix with parent branch (user rule). Clean up after merge.
- NEVER touch/merge/delete anything under `sanfrancisco-*` (another agent's branches/worktrees) or any ref you didn't create.
- NEVER `git add -A` / `git commit -a` — stage explicit paths.
- `git checkout -- uv.lock` if it shows dirty (incidental churn, not ours).
- Do NOT push to origin. `origin/decodex` is pinned at `046a80a` (planning checkpoint); all engine work is LOCAL only until the user says push.
- Shell output occasionally garbles/duplicates under load — recover by re-probing with `echo MARKER` / writing results to /tmp files and reading them.

## KEY TECHNICAL FACTS
- `browser_use_store::Store` (rusqlite `Connection`) is **!Sync** — any SQLite-touching async code must use a dedicated OS thread + channel OR `Arc<Mutex<Store>>` + spawn_blocking; do NOT share `Arc<Store>` across tokio tasks. (B3 store-sink uses the dedicated-thread pattern; B4 uses Arc<Mutex>+spawn_blocking.)
- Real dep types: `browser-use-protocol` HAS `EventRecord` (lib.rs:57: seq,id,session_id,ts_ms,event_type,payload), `ModelUsage` (:134), `ModelEvent`, `ToolSpec`, `ToolCall`. `browser-use-store` HAS `Store`, `StoredEvent`, `StoreNotification` enum (:64), `StoreNotifier=mpsc::Sender`. `browser-use-llm`: `schema::{LlmRequest,Message,ContentPart,ToolDefinition,ToolChoice,LlmEvent,Usage,FinishReason}`, `route::{ModelClient(struct, not trait),Route,Protocol}`, `providers::{OpenAi,Anthropic,OpenAiCompatible}`, `tool`, `tool_runtime`. Agents keep guessing non-existent symbols (`provider::`, `route::EventStream`, `schema::ToolSpec`, `ModelClient`-as-trait) — tell each agent to READ the real files first.
- The tool seam (`tools/{runtime,approval,sandbox,orchestrator}.rs`): a tool impls `Approvable<Req>` + `Sandboxable` + `ToolRuntime<Req,Out>`; ToolError variants are `Rejected(String)`/`Sandboxed(SandboxDenial)`/`Other(anyhow)` — NO Timeout/Invalid. SandboxType is `{None, Restricted}` only. `shell.rs` is the canonical template.
- Toolchain: stable rust 1.95; `i64::div_ceil` is UNSTABLE (spell out `(b+3)/4`); `cargo clippy` NOT installed (use fmt+test only). `cargo fmt -p browser-use-agent` before every commit.
- Tests run network-free via scripted fakes (ScriptedTransport/ScriptedSamplingDriver/ScriptedRunner/ScriptedTask). SQLite tests use a real `Store` on `tempfile::tempdir()` (allowed — local, not network). `tempfile.workspace=true` is in agent dev-deps.

## PARITY DEBT logged (revisit pre-cutover, not blocking): see STATUS.md tail. Notably: A1 backoff_ms deterministic (B5 adds I/O jitter); A3 non_last_reasoning approx + strip_images removes vs placeholder; apply_patch fuzzy-match + turn_diff_tracker + protected-metadata denylist deferred; shell full tree-sitter denylist deferred (simple heuristic in place); inject vs reconstruct context-builder dedup pending; B5 transport-switch no-op (WS fallback not wired); ForkMode::LastN by-message not by-turn; Summary==All.

## USER PREFERENCES (from memory + this session)
- EXTREME PARITY with codex (mechanism/heuristics/values, not "feature exists"). No shortcuts — they killed the no-LLM compaction. Default to matching codex; only the sanctioned divergences (multi-provider; SQLite write-sink; sync/serial view_image; browser+python tools; drop the codex backend) deviate.
- One sub-agent per feature, file-disjoint, isolated worktrees, merged one at a time. Don't swarm the engine (serial). First-principles depth.
- Consolidate into docs, not chat walls. Keep chat replies tight.
- Loud, casual, impatient; wants relentless autonomous progress but HONEST status (never fake a checkmark). They get angry at fake "done". 1M-context model; commits co-authored "Claude Opus 4.8 (1M context)".

## LOOP MECHANICS
You're in a `/loop` (dynamic mode). After each iteration, call ScheduleWakeup (delaySeconds ~1400) with the full /loop prompt verbatim so it re-fires; agent completions wake you sooner via task-notification. To eventually STOP: only when all WPs are [x] or the cutover is [BLOCKED-NEEDS-HUMAN] and nothing else is runnable. The standing /loop prompt is in the conversation; re-pass it each wake.
