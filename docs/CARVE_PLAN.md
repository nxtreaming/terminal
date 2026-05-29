# CARVE_PLAN.md — Phase 0.1 module split of `browser-use-core/src/lib.rs`

Execution spec for carving the ~47k-line `crates/browser-use-core/src/lib.rs` into
submodules **one compiling commit at a time** (behavior-preserving code motion only).

## Rules (non-negotiable)
- **One module per commit.** After each extraction, `cargo check -p browser-use-core` MUST be green before committing.
- **Code motion only** — no logic changes, no refactoring. Behavior must be byte-identical.
- **Visibility:** upgrade moved private items to `pub(crate)` as needed; only `pub` for genuinely external API. Re-export from `lib.rs` so the external surface is unchanged.
- **Never leave the tree broken.** If a module can't be made to compile after reasonable effort, leave it in `lib.rs`, note it, move to the next independent module.
- **Git discipline:** stage only `crates/browser-use-core`, `Cargo.toml`, `Cargo.lock`. Never `git add -A` / `git commit -a` (other crates are being worked in parallel).
- Keep `#[cfg(test)]` blocks with their code.

## Modules (extract in this order — most independent first)

| # | Module | Lines (lib.rs) | Depends on | Notes |
|---|--------|---|---|---|
| 1 | `constants` | 55–251 (~220) | none | pure `const`/`static`; trivial leaf |
| 2 | `events` | ~650 | constants | event builders, lifecycle, turn metadata → `pub(crate)` |
| 3 | `auth` | ~1100 | constants, Store | Claude-Code/Codex OAuth, token mgmt |
| 4 | `images` | ~450 | constants | screenshot/prompt_image output, image budgets |
| 5 | `providers_glue` | ~1300 | constants, auth | provider config, stream assembly, rate limits |
| 6 | `context` | ~1000 | constants, events | context manager, `*_context_message` builders, citations |
| 7 | `goals` | ~1100 | constants, context | goal state, budget accounting, steering prompts |
| 8 | `persistence` | ~1100 | constants, session | history.jsonl, Store interaction |
| 9 | `compact` | ~900 | constants, context, session | compaction, summarization, drop-oldest |
| 10 | `session` | ~1300 | constants, events, persistence | resume, rollback, fork |
| 11 | `agents_md` | ~1300 | constants, plugins, hooks, skills | config load + layer merge |
| 12 | `hooks` | ~1000 | constants, context | hook execution + outcomes |
| 13 | `prompts` | ~600 | constants | instruction templates/builders |
| 14 | `skills` | ~1500 | constants, context, plugins | discovery, render, budgeting, aliases |
| 15 | `plugins` | ~1700 | constants, skills | manifests, MCP config, apps |
| 16 | `subagents` | ~2200 | all + turn | multi-agent v1/v2, child spawn, roles |
| 17 | `turn` | ~1500 | all others | async turn loop, provider turn, tool dispatch (trunk) |

Target: `lib.rs` shrinks to ~2–3k lines of `mod`/`use` + re-exports.

## Tangles & handling
- **goals ↔ context:** `goal_context_message()` lives in `goals`; `context` imports it.
- **skills ↔ plugins:** `skills` reads `plugins` config (immutable ref) → no cycle.
- **subagents ↔ turn:** `subagents` imports `turn` (child runs are sub-turns); `turn` calls subagent handlers as tools.
- **Do NOT further split these giant fns (leave intact):** `run_loaded_session_with_provider` (~3204), `start_provider_turn_with_retries` (~4587), `dispatch_tool_calls_for_turn_with_streaming` (~12307), `update_parent_from_child_run_with_hooks` (~22938), `load_agents_md_context_for_cwd_with_run_options` (~15670).

## Gate (Phase 0.1 done when)
- All extractable modules moved; `cargo check` + `cargo test -p browser-use-core` green; `cargo fmt` clean; no `pub` leakage of internal types; behavior unchanged.

> `tools/*`, `mcp.rs`, `prompt_image.rs` are already separate files — not part of this carve.
