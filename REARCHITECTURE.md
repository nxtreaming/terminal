# browser-use terminal — Re-architecture plan & system map

> One source of truth for the de-codex / async / multi-provider rework.
> Branch: `decodex` (worktree `terminal-decodex`, off `main`).
> Replaces the long chat thread — read the TL;DR, dive into sections as needed.

---

## TL;DR (read this first)

**What the terminal is today:** a browser-first agent terminal that *feature-ported* a large amount of codex's machinery (it didn't start as a fork, but the core now overlaps heavily with codex). It is written as **one ~47k-line synchronous `lib.rs`** plus a few crates. codex, by contrast, is **async (tokio) across dozens of small crates**.

**Everything that differs sorts into three buckets:**

- **① ADDED on top of codex (our product):** browser tools + JS harness, Python worker, **multi-provider models** (OpenAI / Anthropic+Claude-Code / Ollama / OpenRouter / DeepSeek — codex is OpenAI-only), a deliberately **sync/serial `view_image`**, **everything dumped to SQLite** for debugging, a `done` tool, a dataset/eval harness.
- **② DROPPED from codex (the gaps):** the entire **safety substrate** (OS sandbox + approvals + guardian reviewer + network firewall + command policy), **MCP over HTTP/OAuth**, code-mode, richer multi-agent roles, stronger rollout.
- **③ SHARED (ported, both have):** skills, MCP (stdio), subagents (v1+v2), goals/budget, compaction, plan tool, review mode, tool-search, @-mentions. We have *full* versions of these (details in the appendices).

**The plan:**
1. ✅ Worktree/branch ready (`terminal-decodex`).
2. Go **async/tokio + modular crates** (adopt codex's architecture).
3. **Multi-provider, opencode-style** (§3): one *protocol* per wire-format × thin per-vendor configs, over a typed message/event model. Replaces the 4 hand-written sync providers.
4. **De-codex everything** (§4): dropping the codex/ChatGPT backend *is* the de-branding — its headers/OAuth/identifiers disappear with it. Rename the rest to browser-use.
5. **Keep:** SQLite-everything, browser tools, sync image-read, prompts (de-branded), the event-sourced goal/resume design.
6. **Add a safety "seam" now** (even as a no-op orchestrator) so the sandbox/guardian drop in later without re-touching every tool (§5).

---

## Guiding principle: EXTREME PARITY

The bar is **behavioral parity with codex**, not "the feature exists." For every engine subsystem, the rebuilt version must match codex's **mechanism, heuristics, thresholds, and outputs** — verified by parity tests — unless a divergence is *explicitly* listed below as intentional.

- **Match codex exactly:** turn loop, context management (incl. **real** token accounting), **compaction** (always model-based — the no-LLM dump is removed), tool semantics/specs/truncation, sandbox+approvals+guardian, hooks, MCP feature set, subagent semantics, goals/budget, prompts (text, de-branded), rollout/resume behavior.
- **Intentional divergences (preserve, justify, test):** (1) **multi-provider** instead of OpenAI-only; (2) **SQLite as a write-mostly durability/observability + resume sink** — dump to it, never poll/read it on the runtime hot path (in-memory state is authoritative; read SQLite only on resume) — instead of rollout files; (3) **sync/serial `view_image`** for ordered screenshots; (4) the **browser/python tool surface**; (5) drop the **codex/ChatGPT backend** (server-side, can't use). Everything else = parity.

> "It's not enough to say the feature exists — there has to be feature parity." Each work package proves it with tests (see `IMPLEMENTATION_PLAN.md`).

> Companion doc: **`IMPLEMENTATION_PLAN.md`** — the phased, parallel-safe, sub-agent execution plan.

## 1. Where we are today

- **Lineage:** feature-ported from codex. Evidence in-tree: codex event names (`task_started`/`task_complete`/`turn_aborted`), settings (`codex.installation_id`), headers (`x-codex-*`, `x-openai-subagent: collab_spawn`), `<oai-mem-citation>` parsing, `multi_agent_v2`, goals/personality/collaboration, the byte-identical compaction summary prefix, the V4A `apply_patch` grammar, and codex-derived prompt files (`codex-models.json`, `codex-model-fallback-prompt.md`, `review-prompt.md`, `compacted-context-system.md`).
- **Structure:** core is `crates/browser-use-core/src/lib.rs` (~47k lines) + `tools/{command,files,mod,agent_env}.rs` + `mcp.rs`. Satellite crates: `browser-use-providers` (model client), `browser-use-browser` (CDP/JS harness), `browser-use-python-worker`, `browser-use-store` (SQLite), `browser-use-protocol`, `browser-use-tui`, `browser-use-cli`.
- **Sync foundation:** `crates/browser-use-core/src/lib.rs` has **0** `async fn`/`.await`/`tokio`; providers use `reqwest::blocking` (~35 sites). This is the single biggest structural difference from codex and the main thing to change.

---

## 2. Target architecture

| Axis | Today (browser-use) | Target | Source of pattern |
|---|---|---|---|
| Concurrency | sync, `reqwest::blocking`, OS threads | **async/tokio**, `Stream` | codex |
| Layout | one 47k-line `lib.rs` | **many small crates** | codex |
| Config dirs | `.browser-use-terminal` (done) | multi-dir, de-codex'd names | — |
| Providers | 4 sync structs over `Vec<Value>` | **protocol × provider**, typed model | opencode (§3) |
| Model API | OpenAI Responses + Anthropic + Chat + codex-backend | drop codex backend; keep the rest | §4 |
| Safety | none (unconfined) | **orchestrator seam now**, sandbox/guardian later | codex (§5) |
| Persistence | SQLite (hot-polled in places) + `history.jsonl` | **write-sink only**: dump for durability/debug/resume; in-memory runtime state; read SQLite only on resume (never poll it) | — |

---

## 3. Provider layer — opencode-inspired design (the multi-provider rebuild)

opencode's LLM core (`/home/exedev/repos/opencode/packages/llm`, TS/Effect — see its `AGENTS.md`) beats both codex's single fused client and our current `trait ModelProvider`. Three ideas to port:

**(a) Two orthogonal axes: Protocol (wire format) × Provider (vendor deployment).**
- `protocols/` = one impl per wire format: `openai-responses`, `openai-chat`, `anthropic-messages` (+ gemini/bedrock later).
- `providers/` = thin facades that pick a protocol + auth + base URL. All OpenAI-compatible vendors (Ollama/OpenRouter/DeepSeek/Together/Groq) **reuse `openai-chat` verbatim** — each is a 5–15 line config entry, not a 300-line clone.
- A `Route` composes four small values: **Protocol · Endpoint · Auth · Framing**.
- This collapses our current `messages_to_responses_input` / `messages_to_chat_messages` / `messages_to_anthropic_messages` triplication into ~3 protocol `build_body` impls.

**(b) A typed canonical model** (replace `ProviderTurn.messages: Vec<serde_json::Value>`): `LlmRequest` / `Message` / `ContentPart` (tagged enum `Text|Media|ToolCall|ToolResult|Reasoning`) / `ToolDefinition` / `ToolChoice`, and an `LlmEvent` stream with proper `start/delta/end` lifecycle. Each part carries an open `provider_metadata` escape hatch (Anthropic thinking signatures, OpenAI encrypted reasoning).

**(c) Two shared stream helpers** (port as logic): `Lifecycle` (well-formed `step-start → delta* → step-finish`, auto-close dangling blocks) and `ToolStream` (accumulate streamed tool-arg JSON keyed by provider-local id; handle "identity on first delta" vs "explicit start event"). These keep every protocol's `step` tiny.

**Rust/tokio translation (drop Effect entirely):**
- `Effect.Effect<A,E,R>` → `async fn -> Result<A, LlmError>`; `Stream<LLMEvent>` → `impl Stream<Item = Result<LlmEvent, LlmError>>`.
- Keep `Protocol::step` a **pure sync fn** `(&mut State, ProviderEvent) -> Result<Vec<LlmEvent>>` — only transport is async ⇒ trivially testable + a `prepare()` (compile-body-without-send) seam for golden tests.
- `Schema` → `serde` + `schemars` (tool JSON-Schema gen + input validation). DI `Layer`/`Context` → plain constructor injection of an `HttpClient`.

**Proposed crate (`browser-use-llm`, absorbing `browser-use-providers`):**
```
schema/    request.rs · event.rs · ids.rs · options.rs · error.rs
route/     protocol.rs(trait) · endpoint.rs · auth.rs · framing.rs · client.rs · executor.rs(retry+redaction)
protocols/ openai_responses.rs · openai_chat.rs · anthropic_messages.rs · utils/{lifecycle.rs, tool_stream.rs}
providers/ openai.rs · anthropic.rs · openrouter.rs · openai_compatible.rs(+ {provider,base_url} table)
tool.rs · tool_runtime.rs
```
Keep `browser-use-protocol`'s `ModelEvent` as the *outer* boundary; convert `LlmEvent → ModelEvent` at the edge (enriched with lifecycle so the TUI gets clean block boundaries). Keep the model **catalog** (capabilities/cost/limits) *separate* from the protocol crate (opencode keeps a models.dev-style layer decoupled from execution).

**Reuse from current code:** typed `ProviderError`/`ProviderErrorKind`, `ProviderCommandAuth` (token caching/refresh), the `ModelCatalog` (reasoning levels/service tiers).

**Adopt:** protocol×provider split · typed model · Lifecycle/ToolStream · composable `Auth` (`bearer`/`header`/`env`/`and_then`/`or_else`) · hardened executor (typed reasons, `Retry-After`-aware backoff, dual-vendor rate-limit headers, secret redaction) · config-driven OpenAI-compat table · `prepare()` + cassette tests · unified `Usage` with non-overlapping breakdown.
**Skip for v1:** Bedrock binary framing, WebSocket Responses, full hosted-tool matrix, remote catalog fetch (the `Framing`/`Transport`/`provider_metadata` seams let these slot in later).
**Avoid:** fusing protocol+endpoint+auth+transport into one client; keeping `Vec<Value>`; staying `reqwest::blocking`; porting Effect machinery literally.

---

## 4. De-codex plan — two categories, don't conflate them

**(a) Internal branding — rename freely:** dir-name constants (`.browser-use-terminal` already done), event names (`task_started`…), filenames (`codex-models.json`, `codex-model-fallback-prompt.md`), struct/const names (`CODEX_*`). Cosmetic.

**(b) Wire identifiers tied to the codex/ChatGPT backend — *remove with the backend*, don't rename:** `x-codex-installation-id`, `x-codex-beta-features`, `x-codex-turn-metadata`, `chatgpt-account-id`, `codex.installation_id`/`window_id`, `<oai-mem-citation>` handling, the whole `CodexResponsesProvider` + `CODEX_OAUTH_CLIENT_ID` / `CODEX_REFRESH_TOKEN_URL` / `LLM_BROWSER_CODEX_*` env vars. Since we can't use codex's server, **drop the codex backend provider entirely** — its headers/OAuth/identifiers disappear with it. De-branding and dropping-the-backend are the *same* task. After that, only internal names remain, which we rename freely.

---

## 5. Gaps to (re)build — prioritized

Almost every codex-only subsystem hangs off **one missing substrate: a unified, *enforced* approval + sandbox + network safety layer.** Today browser-use commands run via bare `std::process::Command` with **no** seccomp/landlock/seatbelt/bwrap, no approval round-trip, no process-hardening; the only guard is a tree-sitter denylist that blocks `rm -f`/`rm -rf`. `sandbox_permissions` is parse-only prompt text.

**Key insight for sequencing:** codex's `ToolOrchestrator` (approval → sandbox → retry → network-approval) is the **spine every tool routes through**. Since we're rebuilding tools on tokio anyway, **build the orchestrator/runtime-trait seam from day one** (with `SandboxType::None` + auto-approve), so the real sandbox/guardian drop in later without re-threading every handler.

**Dependency-ordered rebuild:**
1. **async/tokio core + provider crate** (§3).
2. **Tools on an orchestrator seam** (`Approvable`/`Sandboxable`/`ToolRuntime` traits; sandbox=None stub). Also *unify exec*: codex routes shell/exec/apply_patch/mcp through one orchestrator; our `unified_exec` PTY model is ported but bypasses it.
3. **MCP HTTP/OAuth/elicitation transports** (use `rmcp-client`-style) + **connectors** — to reach remote/hosted MCP servers (GitHub/Linear/Sentry). Currently stdio-only.
4. **Real OS sandbox backends** (seatbelt/landlock+seccomp/bwrap/windows) + `process-hardening`.
5. **Guardian** (LLM-as-approval-reviewer: fail-closed, circuit breaker on repeated denials, ephemeral forked reviewer session) + **`PermissionRequest` hooks** (deterministic allow/deny before the LLM).
6. **execpolicy** (Starlark allowlist DSL + persisted amendments) + `command_canonicalization`; **managed network proxy/MITM** + deferred network approval.
7. **Secondary:** `code_mode` (V8 tools-as-code), `agent_jobs` (CSV fan-out), role-as-config-layer for subagents, `codex_delegate` shared-service subthreads, `memories` ingestion, `session_startup_prewarm`.

**Also weaker than codex (harden later):** rollout/thread_manager (we replay a JSON event log; codex has first-class `RolloutRecorder`/`ThreadManager` with archival/fork/resume/truncation); hooks (we lack `PermissionRequest` event + `Prompt`/`Agent` handler kinds); multi-agent roles (we lack codex's role-as-config-layer + spawn-depth limits).

---

## 6. What browser-use has on top of codex — PRESERVE these

- **Multi-provider `ModelProvider`** + provider-neutral turn/event (genuinely novel; codex is Responses-only, even routes Ollama through Responses).
- **OpenAI-compatible Chat backend** (Ollama/OpenRouter/DeepSeek/Fireworks; per-vendor quirk toggles) — codex deleted the chat wire API.
- **Anthropic Messages + Claude-Code OAuth:** PKCE login, 401 refresh+rotation, identity spoofing ("You are Claude Code…"), tool-name canonicalization (read→`Read`, shell→`Bash`), system-block placement, freeform-tool→JSON downgrade.
- **Sync/serial `view_image`** (intentional): blocking read + never parallel with browser actions, so screenshots are observed in order. (codex's is async + parallel-safe.)
- **Python worker tool** (persistent subprocess, returns text/artifacts/images/browser_events).
- **Browser tool surface:** `browser_execute/observe/cancel/status/configure/recover` + hidden `browser` cmd tool + screenshot/`prompt_image` pipeline + per-domain browser profiles + the 16 interaction-skills.
- **SQLite as a write-only durability/debug/resume sink** — dump everything for debuggability + resume, but keep runtime state in-memory and never read SQLite on the hot path (see Appendix G). *(Refines "SQLite-everything": persist to it, don't load from it at runtime — otherwise it's slow as hell.)*
- **`done` tool** (explicit completion), **dataset/eval harness** (CLI), **named context-message injection** system, **PostHog analytics**, generated-image artifacts.

## 7. Ported subsystems — audit to PARITY (not "leave alone")

These already exist in some form, but under the parity rule each must be **audited against codex and brought to exact behavioral parity** (many are simplified/diverged today): `tool_search`, `turn_diff_tracker`, `shell_snapshot`, multi-agent v1+v2, the goal tool, **compaction** (currently diverged — see §A), `plugin://`/`app://`/`skill://` mentions, `request_user_input`, hosted `web_search`, review mode, shell detection, image-detail handling. "Present" ≠ "parity" — each gets a parity design note + tests in `IMPLEMENTATION_PLAN.md`.

---

# Appendices — system inventory (reference)

## A. Agent loop · context · compaction

**Loop:** today a bounded sync loop `for turn_idx in 0..max_turns` (default **80**, hard `bail!` on overflow); codex is an unbounded async `loop{}` driven by `needs_follow_up`. Tool dispatch batches contiguous parallel-safe calls onto **OS threads**; codex uses `FuturesOrdered` + an `RwLock` (read=parallel, write=serial). Abort is cooperative store-seq polling; codex uses a `CancellationToken` tree.

**Context:** **estimated** tokens (serialize to JSON, ÷4) vs codex's **real** API token counts. Image/reasoning/encrypted byte estimators are ported verbatim. Named context-message injection (`workspace/permissions/goal/personality/collaboration/hook/mention/generated_image`) vs codex's `reference_context` diff.

**Compaction:** default **no-LLM structured dump** (model path exists but off); single summary prefix byte-identical to codex; preserves recent user turns up to `min(20k, ctx/3)` tokens (codex uses full 20k); codex additionally has *remote* compaction (drop — server-side).

**Key constants (browser-use → codex):**

| Constant | browser-use | codex |
|---|---|---|
| chars/token | `APPROX_CHARS_PER_TOKEN=4` | `4` (bytes) |
| max context | `DEFAULT_MAX_CONTEXT_CHARS=240_000` (≈60k tok) | per-model `auto_compact_token_limit` |
| tool output | `DEFAULT_TOOL_OUTPUT_TEXT_TOKENS=2_500` (×1.2 serialize) | `Bytes(10_000)` ≈2500 tok (×1.2) |
| MCP cap | `MCP_EVENT_RESULT_MAX_CHARS=20_000` (event-log only) | `Bytes(1024)` (model-facing) |
| image budget | `IMAGE_CONTEXT_BUDGET_TOKENS=2_000` | none (real bytes) |
| history soft cap | `MESSAGE_HISTORY_SOFT_CAP_RATIO=0.8` (disk) | none |
| compact user budget | `COMPACT_USER_MESSAGE_MAX_TOKENS=20_000` → `min(., ctx/3)` | `20_000` |
| turn bound | `max_turns=80` (hard fail) | unbounded |
| stream retries | `DEFAULT_STREAM_MAX_RETRIES=5`, `MAX=100` | same |

## B. Tools · exec · sandbox

- **Dispatch:** giant `match` over `ToolHandlerKind` enum vs codex's trait-object registry. Both share `ToolExposure {Direct, Deferred, DirectModelOnly, Hidden}` and bm25 tool-search gating.
- **Model-visible tools:** shell (`shell_command` / `exec_command`+`write_stdin`), `apply_patch`, `view_image`, goals, `update_plan`, `request_user_input`, browser family, `done`, multi-agent, MCP. **Not real specs (prompt-described, name-fallback dispatch):** `python`, `read_file`/`search_files`/`list_files`. **No write/edit tool** — all edits via `apply_patch` (V4A). `request_permissions` is **codex-only** (we only have a permissions *context message*).
- **Shell exec:** bare `std::process::Command` (pipe + `portable_pty`), default 10s timeout (no upper cap), 1 MiB/stream output cap then token-budget truncation. **No OS sandbox at all.** `apply_patch` and `view_image` resolve absolute paths and `..` escapes permissively; `cwd` is only the base for relative paths.
- See §5 for the full safety gap and rebuild order.

## C. Prompts · providers · auth

- **System prompt:** `BROWSER_AGENT_IDENTITY_PREAMBLE` → selected codex model instructions (from bundled `codex-models.json` via `ModelCatalog`) → terminal tooling amendment → `browser-agent-system.md` contract → 16 appended `interaction-skills/*.md`. Personality enum `None|Friendly|Pragmatic`.
- **Fallback prompts:** `codex-model-fallback-prompt.md` (trimmed copy of codex `gpt_5_1_prompt.md`) for codex-family; `model-fallback-prompt.md` (neutral) otherwise.
- **Review prompt:** free-form `[P0]–[P3]` prose (codex mandates a strict JSON schema).
- **Providers/auth:** see §3 (design) and §6 (Anthropic/Claude-Code specifics). Retry numbers match codex (`req=4`, `stream=5`, cap `100`). Token storage = our SQLite `Store` under `auth.*` keys + `LLM_BROWSER_*` env ladder (codex uses `auth.json`+keyring).

## D. Skills & plugins

Three systems: (1) **local skills**, (2) **plugins**, (3) **browser interaction-skills** (compiled-in, always appended to the browser system prompt; no gating).

- **Local skills:** `SKILL.md` with YAML frontmatter (`name`, `description`/`metadata.short-description`, `policy.allow_implicit_invocation`); discovery across roots with `scope_rank` (SYSTEM<REPO<USER): `<home>/skills`, `.agents/skills`, `.tmp/skills` (bundled), each enabled plugin's skill root, `/etc/.../skills`, project `.agents/skills` + `.browser-use/skills`. Recursion depth 5, dedup by canonical `SKILL.md` path.
- **Instruction injection:** `<skills_instructions>` block, **token-budgeted at ~2% of context window** (or 8k chars), with three-tier rendering (full → per-description char budget → minimal lines) and **alias compression** (`r0/r1/…` skill roots) when over budget; emits omission/truncation warnings. Plugins inject `<plugins_instructions>` (240-char descriptions, no budgeting).
- **Plugins:** local-only (no network/marketplace install). `name@marketplace` resolves to cache/curated dirs; manifest `.browser-use-plugin/plugin.json` (or `.claude-plugin/`). Each enabled plugin can contribute **skills**, **MCP servers** (merged into config; `type`/`oauth` fields stripped), and **hooks** (merged with `HookSourceKind::Plugin`).
- **Mentions:** `$Name` (skill), `[label](app://…)`/`(plugin://…)`/`(skill://…)` → reads `SKILL.md` into a `<skill>` user message / emits a `typed_mention_context` developer message.
- **No caching/hot-reload** — discovery re-runs each turn. No install/list tools.

## E. MCP

- **stdio-only JSON-RPC** (`crates/browser-use-core/src/mcp.rs`, ~1912 lines). Multi-server via `[mcp_servers.<name>]` (command/args/env/cwd, `required`, `supports_parallel_tool_calls`, startup/tool timeouts, `enabled_tools`/`disabled_tools`).
- Handshake pins `protocolVersion=2024-11-05`, sends `capabilities:{}` and **ignores** server capabilities. Connections cached per `(session, server)`, reused across calls; transport errors reset+respawn; timeout SIGTERM→SIGKILLs the child. stderr ring-buffered (8k) and attached to errors.
- **Tools:** `tools/list` → namespaced `mcp__<server>__<tool>` (or flat for non-namespace models); read-only hint drives parallel-safety. **≥100 tools ⇒ all Deferred** behind tool-search. **Resources:** `resources/list` + templates + read, with cross-server aggregation and pagination.
- **Result handling:** prefixes wall-time, passes `structuredContent` or content array; images → `input_image` data-URLs with `_meta["codex/imageDetail"]` override. Event-log copy truncated at 20k chars (model-facing output untouched).
- **No approval/consequential gating.** **Not supported:** HTTP/SSE/streamable, OAuth, elicitation/sampling/roots/prompts, capability negotiation, list-changed subscriptions.
- ⚠️ **Bug to fix in rebuild:** plugin-declared MCP servers can carry `type` (HTTP) + `oauth`, but both are silently stripped/ignored → such a server quietly does nothing.

## F. Subagents / multi-agent

- **Two families.** V2 (default-on; set `features.multi_agent_v2.enabled = false` to disable): `spawn_agent`, `wait_agent`, `send_message` (no turn), `followup_task` (triggers turn), `list_agents`, `close_agent` — **Direct** tools, hierarchical `/root/...` task-name routing, `fork_turns` (`none`/`all`/int). V1: `spawn_agent`, `send_input` (+`interrupt`), `resume_agent`, `wait_agent`, `close_agent` — **Deferred** (tool-search-gated only), opaque 12-hex agent ids, `fork_context`. Master switch `features.multi_agent`; review mode disables both.
- **Spawn:** each child is a **separate session** (own 12-hex id, parent link, inherited cwd) created in the store; runs on a **background OS thread** via `ChildAgentRunner`. Roles (`default`/`explorer`/`worker` + config-file roles) set model/effort/tier; nicknames drawn from `agent_names.txt` (108 names) with collision suffixes. Briefed via `helper-session-identity.md` (+ `helper-session-inherited-context.md` when not full-fork).
- **Wait/messaging:** `agent_messages` mailbox table; **`wait_agent` polls every 50ms** (V2: mailbox arrival → summary; V1: terminal status → result). `followup_task`/`send_input` deliver via store + `agent.message` event. Completion sends a `<subagent_notification>` mail to the parent.
- **Limits:** **max 4 concurrent** threads/session; wait timeouts min 10s / default 30s / max 3600s. Tree walked via parent links; cap enforced at spawn. **Done-gate:** a parent can't finish while descendants are active (unless `finish_and_close_children`).
- **Headers:** child sessions send `x-openai-subagent: collab_spawn` + `x-codex-parent-thread-id` (drop with the codex backend).

## G. Goals & persistence/resume

- **Goals are event-sourced** (no goal table): replay `goal.created`/`goal.updated` to reconstruct `ThreadGoalSnapshot`. Statuses: `active`/`complete`/`blocked` (model-set) + `budget_limited`/`usage_limited` (system-set). **Billable tokens** = `(input − cached_input) + max(output,0)`; tracked vs a baseline captured at creation, summed from `goal.accounted` events.
- **Steering:** `goal.continuation_requested` fires when the model tries to end a turn while the goal is still `active` — it **prevents turn-end** and re-injects the continuation prompt, forcing work until `update_goal complete`/`blocked` or a budget/usage limit. `goal.budget_limit_steering_requested` injects a wrap-up message once when budget is hit.
- **SQLite store** (`crates/browser-use-store`, `state.db`, WAL): 8 tables — `sessions`, `events` (append-only, the source of truth), `artifacts`, `runs`, `agent_edges` (parent→child tree), `agent_messages` (mailbox), `app_settings` (KV), `schema_migrations`. `status_for_event` maps event types to session status.
- **Home dir** (`.browser-use-terminal`, or `$BROWSER_USE_TERMINAL_HOME`): `config.toml` (+ profile/managed/system layers), `models_cache.json` (TTL 300s), `skills/`, `.agents/`, `plugins/cache`, `history.jsonl`, `state.db` + `artifacts/<session>/`.
- **Resume = load session by id from the same dir → replay events** (`provider_messages_from_events`, honoring compaction checkpoints + `session.rollback`). Restores conversation, goal+budget (event-sourced), model/provider/personality/collaboration (from `session.config_snapshot`), sub-agent tree, browser prefs, cwd. **Fork:** sub-agent fork (`fork_turns`) keeps only system/developer/user + final-answer messages; in-session rollback drops last N user turns.
- **`history.jsonl`** is the *only* conversational data outside SQLite — a global, byte-capped (`soft_cap=0.8`), advisory-locked (10×100ms) up-arrow input-recall log; **not** used for resume.

> **Target refinement:** today some paths *read* SQLite hot (e.g. subagent wait polls every 50ms — Appendix F). In the rebuild SQLite is **write-only at runtime** (durability/observability sink): in-memory state is authoritative, coordination is in-memory (event-notify), and SQLite is read **only on resume**.

---

## Reference repos
- codex (architecture/safety/async reference): `/home/exedev/repos/codex/codex-rs/core` (ignore server/cloud crates: `app-server*`, `cloud-tasks*`, `backend-client`, `responses-api-proxy*`, chatgpt cloud login).
- opencode (provider design reference): `/home/exedev/repos/opencode/packages/llm` (+ its `AGENTS.md`).
- browser-harness-js: the browser interaction layer — treated as a black box here.
