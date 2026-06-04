# Live Runtime Inversion Status Audit

This file is the current truth for the `codex/live-runtime-rewrite` branch after
the latest runtime/mailbox hardening commits. The long checklist in
`docs/agent-design/live-runtime-inversion-checklist.md` is still the target, but
it is not all implemented yet.

## Current Shape

```text
TUI / CLI / Python SDK
        |
        v
RuntimeAgentExecutor / SDK JSON-RPC
        |
        v
RuntimeHandle::run_agent
        |
        +-- barrier: agent.input.accepted
        +-- barrier: agent.started / agent.turn.started
        +-- claims BrowserManager metadata lease when browser_id exists
        |
        v
RuntimeTurnDriver
        |
        v
existing agent crate model/tool loop
        |
        +-- LiveTurnState
        |     +-- runtime-backed fresh input consumption
        |     +-- runtime-backed mailbox delivery/consume
        |     +-- Store/Journals still used for durable prompt/history
        |     +-- compaction/token state still local to agent crate state
        |
        +-- provider/tool registry
              +-- runtime event sink when runtime is present
              +-- some resource ownership still provider/session scoped

BrowserUseRuntime
        |
        +-- AgentManager / AgentThread
        |     +-- in-memory mailbox with seq wakeups
        |     +-- AgentLiveState counters/cursors
        |     +-- AgentResourceSet skeleton
        |
        +-- BrowserManager
        |     +-- BrowserHandle lease/status/action lock
        |     +-- active browser_script run projection
        |
        +-- JournalSink / StateIndex
              |
              v
            SQLite debug journal + replay source
```

## What Is Implemented

- Root agent input is accepted inside `RuntimeHandle::run_agent` before
  `agent.started`, with barrier journal semantics.
- Runtime-backed runs no longer enqueue live mailbox rows into
  `agent_messages`; live smokes show `agent_messages` stays empty.
- Child completion mail is non-triggering and delivered through the runtime
  mailbox. `wait_agent` observes mailbox sequence changes and records
  `after_seq`, `mailbox_seq`, and `timed_out`.
- Runtime mailbox drain now journals prompt-projection rows such as
  `agent.mailbox_input`, `session.followup`, and `agent.turn_queue_drained`
  through `RuntimeHandle::append_observed_session_event`, so live mailbox
  delivery no longer writes those rows directly through `Store`.
- Runtime mailbox prompt projection now fails closed: if the runtime journal
  append for the prompt-visible `session.followup` or `agent.mailbox_input` row
  fails, that mailbox content is not returned to the model prompt.
- Runtime-backed compaction progress, errors, token usage, context-window-full
  markers, and `session.compacted` checkpoints now append through
  `RuntimeHandle::append_observed_session_event` when a runtime handle exists.
- Runtime-backed prompt history reads, pre-turn replay cursor calculation, and
  previous-model downshift compaction setup now prefer
  `RuntimeHandle::events_for_session` before falling back to the compatibility
  `Store` read.
- Per-agent live snapshots now include token usage plus runtime-owned
  compaction window state: ordinal, prefill input tokens, and whether the
  prefill came from an estimate or a server-observed usage sample. Runtime-backed
  `LiveTurnState::token_status` updates that state through `RuntimeHandle`.
- Subagent lifecycle UI events use the runtime-backed event sink when a runtime
  handle exists.
- `session.done` emitted by the turn observer is routed through the runtime
  journal path, and `RunAgentResponse.final_result` is derived from the runtime
  journal.
- CLI child spawn/send/wait paths have runtime-backed live tests and guards
  proving they do not depend on store `agent_messages` for live delivery.
- SDK JSON-RPC request lifecycle is hardened for serialized writes,
  cancellation cleanup, notification writes, and pending-future failure on
  close.
- SDK JSON-RPC now exposes `runtime.snapshot` and `agent.snapshot`, forwards
  projected runtime events separately from raw debug events, and Python
  `Agent.stream()` consumes the projected event queue after yielding an initial
  agent snapshot.
- SDK JSON-RPC live projected events are reducer-backed. `agent.run` also
  returns an in-band `final_projected_event` so Python streaming can yield a
  terminal projection before `agent.completed` even if the background event
  forwarder races the JSON-RPC response.
- Projected runtime subscriptions now carry a reducer-backed
  `RuntimeSnapshot`. The reducer materializes child spawn events from their
  payload, keeps parent threads open for child-terminal transcript events,
  updates mailbox counters/cursors, tracks observed tool activity and model
  deltas, projects token usage and final result/failure fields, and tracks
  browser create/claim/release/close state without re-querying SQLite.
- Python SDK tests cover `Agent.add_new_task()` follow-up delivery, asyncio
  cancellation to `agent.stop`, same-browser fail-fast behavior, and concurrent
  different-browser runs.
- Browser leases in `BrowserManager` are barrier-journaled, depth-aware for
  same-agent nested claims, and expose a runtime action-serialization helper used
  by the runtime browser backend. Same-browser conflicting owners still fail at
  the runtime boundary.
- Runtime-attached browser backend resources now use agent-scoped
  `browser.created` and `browser.closed` barrier events before becoming visible
  or being removed from `BrowserManager`. The unattached SDK/global
  `browser.create` path remains best-effort because it has no agent/session
  journal context.
- Browser script start/output/completion/cancellation/failure events now append
  through a browser-scoped runtime journal path. `BrowserHandle` tracks active
  script runs by `run_id`, runtime projection exposes active browser scripts,
  and the runtime browser backend synthesizes lifecycle events while holding the
  browser action lease.
- Runtime session resources are attached to `AgentThread.ToolResourceBag` and
  are cleaned on `close_agent`; provider tests guard the runtime path against
  falling back to global exec/MCP/browser/Python resources.
- The TUI projection cache overlays live session status from the runtime snapshot
  before calling `project_workbench`, so running/terminal runtime status can
  correct stale Store `SessionMeta` without mutating SQLite. Runtime `Created`
  does not resurrect a terminal Store session.
- Replay marks stale live tool resources as lost/orphaned without resurrecting
  process handles. This covers unclosed `exec_command`, `browser_script`,
  Python, and MCP resource starts, and deduplicates `resource.lost` on repeated
  attach/resume.

## What Is Still Not The Final Architecture

- `RuntimeAgentExecutor` still builds a `SharedStore` and supplies a closure to
  `RuntimeHandle::run_agent`. The runtime owns the lifecycle envelope, but the
  reusable turn loop is still implemented in `crates/browser-use-agent`.
- `RuntimeTurnDriver` still calls the existing
  `run_session_once_with_config_with_cancel` path. The turn loop has been
  wrapped by runtime ownership, not moved into `browser-use-runtime`.
- `LiveTurnState` still contains `SharedStore` and reconstructs durable prompt
  history through the runtime journal reader when available, with Store fallback
  for compatibility. Fresh input, mailbox, and compaction window state are
  runtime-backed, but token-status recomputation and token replay still live in
  the agent crate turn state. Runtime-backed compaction checkpoint writes now
  route through runtime.
- `run_session_with_config*` remains as compatibility wrappers over
  `RuntimeHandle::run_agent`. They are not an independent live authority when
  called normally, but they have not been deleted.
- `StoreNotificationWatcher` and `agent_messages` still exist for history,
  replay, and compatibility tests. The live runtime path has guards against
  using them, but the old storage surfaces are not removed.
- `AgentThread.ToolResourceBag` owns unified exec, Python worker, MCP client,
  and runtime browser backend resources for runtime-attached sessions. The old
  `AgentResourceSet` name remains as a compatibility alias. Browser script
  ownership is still mediated through the provider-built runtime browser backend
  rather than a richer `BrowserHandle`.
- `BrowserManager` now owns browser lease depth, action serialization, and the
  runtime-visible active browser script registry. Actual CDP/session execution
  and the concrete script process registry are still held by the runtime browser
  backend resource in the agent provider, not directly by a rich `BrowserHandle`
  with artifacts and crash semantics.
- TUI uses runtime for live cancellation/follow-up/mailbox counts and now
  projects active sessions from cached runtime snapshots at render time without
  mutating Store-derived history. Durable history/sidebar/event text still come
  from SQLite. The SDK consumes projected events for `Agent.stream()`, and
  runtime projection now has a state reducer covering agent status, child state,
  mailbox continuation state, browser state, observed tool activity, model
  stream/thinking deltas, active model request/retry/error lifecycle, token
  usage, and terminal result/failure state.
- Replay materialization restores important mailbox/live counters and marks
  common stale tool resources lost, but full crash recovery still does not
  hydrate every durable graph field or make browser leases/script registries
  first-class runtime resources.

## Verification Passed

- `cargo fmt --check`
- `cargo test`
- `uv run --with pytest python -m pytest -q`
- `scripts/verify-terminal-ui.sh`
- Inspected `/tmp/but-design-loop` deterministic dumps and tmux captures.
- Live GPT-5.5 subagent smoke:
  `spin up 2 subagents ask them whats the capital of france and compare answers`
- Live GPT-5.5 codebase research smoke:
  `can you spin up a few subagents that research this codebase pls`
- Browser boundary smoke:
  `scripts/live-browser-boundary-smoke.sh`
- Direct CLI tool smoke:
  `user-shell` plus `python`
- Live GPT-5.5 model-facing exec/write smoke:
  `exec_command` with `tty=true`, followed by `write_stdin`, producing
  `hello` and `got:hello`.

## Remaining High-Risk Work

```text
1. Move turn-loop ownership deeper into RuntimeHandle/AgentThread.
2. Replace Store-shaped LiveTurnState token recomputation/replay with
   runtime-owned state plus JournalReader replay.
3. Complete RuntimeEventProjection coverage and make it the live TUI authority.
4. Promote BrowserManager from lease/action ownership to the owner of browser
   sessions, concrete script process registries, artifacts, cancellation, and
   crash semantics.
5. Complete replay/crash recovery for durable graph fields that are not yet
   materialized through RuntimeHandle.
6. Add negative barrier tests for remaining critical transitions not already
   covered.
7. Delete or hard-gate old store-first compatibility helpers once all call sites
   are gone.
```

The branch is much more robust than the initial hybrid, and the user-visible
subagent bugs that triggered this work are covered by live smokes. It is not yet
the full final design from the checklist.
