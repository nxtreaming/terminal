# Live Runtime Inversion Implementation Checklist

This is the implementation checklist and completion record for the original
live-runtime rewrite. The branch has completed the inversion: the runtime owns
live agent execution, while SQLite is only persistence, replay, and postmortem
debugging.

## Implementation Status: 2026-06-04

The branch is now in the intended runtime-owned shape for the normal TUI, CLI,
and SDK execution paths. `BrowserUseRuntime` is the live authority for accepted
input, turn status, cancellation, mailbox delivery/wakeups, child status,
browser leases, tool resources, and runtime projections. SQLite remains the
write-through journal, durable replay source, and postmortem/debug database.

```text
TUI / CLI / Python SDK
        |
        v
RuntimeAgentExecutor / SDK runtime context
        |
        v
RuntimeHandle::run_agent(...)
        |
        v
BrowserUseRuntime
  |
  +-- AgentManager / AgentThread
  |     +-- AgentMailbox
  |     +-- AgentLiveStateSnapshot
  |     +-- ToolResourceBag
  |     +-- cancellation status
  |
  +-- RuntimeTurnDriver
  |     +-- existing TurnLoop + SamplingDriver
  |     +-- LiveTurnState
  |     +-- runtime mailbox/follow-up drain
  |
  +-- BrowserManager / BrowserHandle
  |     +-- browser leases
  |     +-- browser_script run registry
  |
  +-- RuntimeEventBus / RuntimeEventProjection
  |
  +-- JournalSink / StateIndex / JournalReader
        |
        v
      SQLite debug + replay journal
```

Important caveat: transcript reconstruction, session metadata, history views,
and child bootstrap context still read/write SQLite journal rows because SQLite
is the durable debug/replay source. Those paths do not schedule live work or
wake a parent by themselves. The implemented guard is that live input, mailbox
wakeups, child completion delivery, cancellation, browser claims, and tool
resources go through `BrowserUseRuntime`.

Latest browser ownership slice: runtime-created browser backends now carry owned
`BrowserSessionRegistry` and `BrowserScriptRunRegistry` instances, so normal
TUI/CLI/SDK runtime paths do not create browser sessions in the global
`browser-use-browser` registries. The old global registries remain only behind
compatibility wrapper APIs for non-runtime direct callers.

## Former Hybrid Removed

```text
TUI / CLI / Python SDK
        |
        v
RuntimeHandle
        |
        v
BrowserUseRuntime
  |       |        |        |
  |       |        |        +-- RuntimeEventBus + JournalSink
  |       |        |                 |
  |       |        |                 v
  |       |        |              SQLite
  |       |        |
  |       |        +-- BrowserManager metadata shell
  |       |
  |       +-- AgentManager / AgentThread / live mailbox / wait_agent
  |
  +-- LiveAgentExecutor
          |
          v
run_session_with_config_with_cancel_and_runtime(...)
          |
          v
StoreTurnState + old TurnLoop + provider/tool registry
          |
          v
Store reads still decide parts of live turn state
```

This diagram is retained as the old shape the checklist was written to remove.
The current code keeps some old public names as wrappers or compatibility
facades, but those wrappers enter `RuntimeHandle::run_agent` before live work is
driven. Store reads remain for replayable transcript/history context, not for
live scheduling or wakeups.

## Required End State

```text
TUI / CLI / Python SDK / future app-server
        |
        v
RuntimeHandle
        |
        v
BrowserUseRuntime
  |
  +-- AgentManager
  |     |
  |     +-- AgentThread
  |           |
  |           +-- LiveTurnState
  |           |     +-- input_queue
  |           |     +-- followup_queue
  |           |     +-- mailbox cursor and consumed seq
  |           |     +-- compaction/token state
  |           |
  |           +-- RuntimeTurnDriver
  |           +-- AgentMailbox
  |           +-- ToolResourceBag
  |           +-- Cancellation tree
  |
  +-- BrowserManager
  |     +-- BrowserHandle
  |           +-- browser/CDP/session state
  |           +-- script run registry
  |           +-- active agent lease
  |
  +-- RuntimeEventBus
  |     +-- RuntimeEventProjection
  |
  +-- JournalSink / StateIndex / JournalReader
        |
        v
      SQLite debug + replay only
```

## Non-Negotiables

- [x] Runtime is the only live authority for turns, input, mailbox wakeups,
      subagent state, browser leases, cancellation, and tool resources.
- [x] SQLite is a write-through journal and replay source only.
- [x] No live path decides work by polling `Store` rows or
      `StoreNotificationWatcher`.
- [x] No accepted user input, mailbox wakeup, child completion, browser claim,
      or terminal status is visible before its barrier journal write succeeds.
- [x] Old public APIs may remain only as wrappers over runtime APIs.
- [x] Unsupported SDK parity fails loudly.
- [x] TUI active sessions render from runtime projection, not store refreshes.
- [x] History, export, postmortem, and debug views can still read SQLite.

## Work Package 0: Baseline And Guardrails

- [x] Keep this work on `codex/live-runtime-rewrite` or rename to a clearly
      scoped runtime-inversion branch.
- [x] Record the current hybrid seams:
      - `crates/browser-use-agent/src/live_executor.rs`
      - `crates/browser-use-agent/src/entrypoint/mod.rs::StoreTurnState`
      - `crates/browser-use-agent/src/tools/handlers/subagent.rs`
      - `crates/browser-use-cli/src/main.rs::spawn_cli_child_agent`
      - `crates/browser-use-tui/src/runtime.rs::spawn_tui_child_agent`
- [x] Add compile-time or test guards that fail if new live code calls
      store-backed wait/mailbox scheduling.
- [x] Keep the existing successful smoke prompt as a regression:
      `spin up 2 subagents ask them whats the capital of france and compare answers`.

## Work Package 1: Runtime API Schema

- [x] Add `RunAgentRequest`:
      - `agent_id`
      - `run_id`
      - `provider_config`
      - `initial_input`
      - `browser_id`
      - `cwd`
      - `cancel_token`
      - `input_source`
      - `resume_mode`
- [x] Add `RunAgentResponse`:
      - `agent_id`
      - `run_id`
      - `final_status`
      - `final_result`
      - `usage`
      - `terminal_event_seq`
- [x] Add `RuntimeHandle::run_agent(request)`.
- [x] Add `RuntimeHandle::submit_input(agent_id, input, trigger_turn)`.
- [x] Add `RuntimeHandle::submit_followup(agent_id, input)`.
- [x] Add `RuntimeHandle::snapshot_agent(agent_id)`.
- [x] Add `RuntimeHandle::subscribe_agent_projection(agent_id)`.
- [x] Make `RuntimeHandle::cancel_run`, `close_agent`, `wait_agent`,
      `send_agent_message`, and browser lease APIs use the same ids and
      lifecycle model.

## Work Package 2: AgentThread State

- [x] Expand `AgentThread` with live turn state.
- [x] Add runtime-owned accepted root input state.
- [x] Add runtime-owned follow-up state.
- [x] Add mailbox delivery cursors:
      - last enqueued seq
      - last delivered seq
      - last consumed seq
      - current-turn vs next-turn delivery phase
- [x] Add per-agent token usage and compaction window state.
- [x] Add per-agent current run state:
      - idle
      - queued
      - running
      - cancelling
      - completed
      - failed
      - closed
- [x] Add per-agent cancellation tree:
      - agent token
      - run token
      - turn token
      - tool tokens
- [x] Add `ToolResourceBag` to `AgentThread`.
- [x] Add runtime snapshots that include live state without querying SQLite.

## Work Package 3: RuntimeTurnDriver

- [x] Route the reusable model/tool loop through `RuntimeTurnDriver` instead of
      letting `run_session_with_config_with_cancel_and_runtime` be live
      authority.
- [x] Create `RuntimeTurnDriver` that receives:
      - `AgentThread`
      - `LiveTurnState`
      - `ToolResourceBag`
      - `RuntimeEventSink`
      - `ProviderRunConfig`
- [x] Make first sampling input come from runtime accepted input, not
      `Store::events_for_session`.
- [x] Make follow-up sampling input come from runtime follow-up queue.
- [x] Make mailbox input come from runtime mailbox delivery, not
      `agent_messages`.
- [x] Preserve existing model/tool fusion behavior.
- [x] Preserve existing compaction behavior, but move compaction state out of
      `StoreTurnState`.
- [x] Preserve existing token accounting behavior, but source live token state
      from `LiveTurnState`.
- [x] Journal model/tool events through runtime before publishing them.
- [x] Flush best-effort deltas before terminal completion.
- [x] Return terminal status through `RunAgentResponse`.

## Work Package 4: Replace StoreTurnState

- [x] Implement `LiveTurnState: TurnState`.
- [x] Port prompt reconstruction to durable event replay helpers.
- [x] Port pending input logic from `StoreTurnState` to `LiveTurnState`.
- [x] Port mailbox current-turn/next-turn logic to runtime mailbox cursors.
- [x] Port active follow-up drain logic to runtime follow-up queue.
- [x] Port compaction checkpoint writes to `JournalSink`.
- [x] Port token status recomputation to live state plus journal replay.
- [x] Remove `StoreTurnState` from the normal live run path.
- [x] Keep any remaining Store-based turn state only under explicit test or
      replay compatibility names.

## Work Package 5: Journal Barriers And Event Contract

- [x] Enforce barrier-before-publish for:
      - session/agent create
      - accepted user input
      - mailbox enqueue
      - mailbox delivered
      - mailbox consumed
      - spawn edge open/close
      - browser claim/release
      - terminal agent status
      - cancellation request
- [x] Emit `mailbox.delivered` when input becomes visible to a turn.
- [x] Emit `mailbox.consumed` when the delivered item has been committed into
      prompt/history.
- [x] Persist consumed seq so resume cannot redeliver old mail.
- [x] Add negative tests where barrier append failure prevents:
      - accepted input success
      - mailbox wake
      - child completion success
      - browser claim success
      - final completion success
- [x] Keep SQLite postmortem complete enough to explain every live transition.

## Work Package 6: Subagents And AgentControl

- [x] Make `spawn_agent` a thin runtime call.
- [x] Make `send_message` a thin runtime call.
- [x] Make `followup_task` a thin runtime call.
- [x] Make `wait_agent` a thin runtime call.
- [x] Make `close_agent` a thin runtime call.
- [x] Make `resume_agent` hydrate through runtime, not direct store scheduling.
- [x] Delete or make unreachable store-backed live wait paths:
      - `StoreNotificationWatcher`
      - store-backed `wait_agent`
      - store-backed mailbox drain as live behavior
- [x] Keep store-backed helpers only for replay/history/compat tests.
- [x] Preserve Codex semantics:
      - child completion mail has `trigger_turn=false`
      - `wait_agent` wakes but does not return child content
      - follow-up/initial delegated mail may use `trigger_turn=true`
      - full-history fork inherits model/type/reasoning and rejects overrides
- [x] Implement strict capacity as the default.
- [x] Implement queued capacity only behind explicit config.
- [x] Ensure completed children remain visible until explicitly closed.

## Work Package 7: ToolResourceBag

- [x] Move `UnifiedExecManager` ownership into `AgentThread.ToolResourceBag`.
- [x] Remove provider-created global exec manager as normal live path.
- [x] Make `write_stdin` validate agent ownership of process ids.
- [x] Move Python worker ownership into `ToolResourceBag`.
- [x] Keep Python worker alive across follow-ups for the same agent.
- [x] Close Python worker on agent close/runtime shutdown.
- [x] Move MCP manager ownership into `ToolResourceBag`.
- [x] Close MCP transports on agent close/runtime shutdown.
- [x] Ensure tool handlers receive resources from runtime/tool context.
- [x] Mark old resources lost/orphaned on crash/resume instead of pretending
      handles survived.

## Work Package 8: BrowserManager And BrowserHandle

- [x] Extend `BrowserHandle` beyond metadata for runtime projection:
      - active agent lease
      - action serialization
      - active browser-script snapshots
      - browser status/projection state
- [x] Move physical CDP/browser process/session state out of
      `browser-use-browser` process globals for runtime-created browser
      backends.
- [x] Move physical browser-script run registry out of
      `browser-use-browser::BROWSER_SCRIPT_RUNS` for runtime-created browser
      backends.
- [x] Move session-layer capture ownership out of process-global capture state
      for runtime-created browser backends.
- [x] Store the concrete browser registries directly on `BrowserHandle` rather
      than on the runtime browser backend resource.
- [x] Make browser tools resolve a `BrowserHandle`, not only a `session_id`.
- [x] Enforce one running agent per browser.
- [x] Allow many browsers in parallel.
- [x] Make `browser_script` start/observe/cancel publish through
      `BrowserHandle`.
- [x] Make `browser_script` start/observe/cancel physically execute without
      global script registries for runtime-created browser backends.
- [x] Journal browser claim/release/start/close as barrier events.
- [x] Journal script output/completion/cancellation in order.
- [x] Make SDK `Browser` map to `browser_id`.
- [x] On crash/resume, mark old browser/script handles lost or orphaned.

## Work Package 9: Runtime Projection

- [x] Implement a real `RuntimeEventProjection` state machine, not just event
      wrapping.
- [x] Projection includes:
      - [x] active agent status
      - [x] live model/tool activity
        - [x] observed tool start/delta/completion state
        - [x] latest model stream/thinking deltas
        - [x] active model request/retry/error lifecycle
      - [x] child agent statuses
      - [x] mailbox continuation state
      - [x] browser status
      - [x] token usage
      - [x] final result/failure
- [x] Projection guarantees snapshot-before-live-event ordering.
- [x] Projection sends final status before `run_agent` resolves.
- [x] TUI consumes projection for active sessions.
- [x] SDK consumes projection for `stream()`.
- [x] Raw runtime event stream remains opt-in for debugging.

## Work Package 10: TUI Integration

- [x] Replace active session rendering from Store cache with runtime projection.
- [x] Keep SQLite reads for history/sidebar/resume only.
- [x] Remove or demote `TUI_LIVE_EXECUTORS`.
- [x] Remove `spawn_tui_child_agent` as a separate Store-first live launcher.
- [x] Route root runs, followups, cancel, close, auth resume, and mailbox
      continuation through runtime APIs.
- [x] Ensure running subagents stay visible during waits and timeouts.
- [x] Ensure completed subagents stay visible until explicit close.
- [x] Ensure wait timeout does not blank child panels.
- [x] Ensure terminal output remains selectable plain text after completion.

## Work Package 11: CLI Integration

- [x] Replace `run_session_via_engine*` with `RuntimeHandle::run_agent`.
- [x] Remove `spawn_cli_child_agent` as a separate Store-first live launcher.
- [x] Live commands use runtime:
      - run
      - followup
      - resume/live continuation
      - cancel/stop
      - close/send/wait/list subagents
      - browser_script
      - Python live execution
      - eval/dataset live execution
- [x] Journal/history commands use Store/StateIndex:
      - show
      - history
      - export
      - inspect
      - cleanup
      - auth/config/profile commands not tied to live handles
- [x] Cleanup commands must not kill unrelated live runtime resources.

## Work Package 12: SDK Integration

- [x] Keep `browser-use-terminal sdk-server --transport stdio`.
- [x] Make SDK `agent.run` call runtime `run_agent`.
- [x] Add SDK event stream notifications from runtime projection.
- [x] Add `Agent.stream()`.
- [x] Add `Agent.stop()` cancellation through runtime.
- [x] Add asyncio cancellation -> runtime cancellation.
- [x] Add follow-up support through runtime input/follow-up queue.
- [x] Add one-browser-one-running-agent enforcement.
- [x] Add different-browser parallel run support.
- [x] Add structured output validation.
- [x] Remove SDK agent runs that scrape CLI output or call old store-first paths.

## Work Package 13: Replay, Resume, Rollback, And Crash Recovery

- [x] Add runtime materializer from `JournalReader` and `StateIndex`.
- [x] Hydrate:
      - root session metadata
      - child tree/spawn edges
      - pending mailbox items
      - consumed mailbox seq
      - goals/followups
      - config/model/provider state
      - durable artifacts
      - transcript/history
- [x] Do not hydrate live process handles.
- [x] Journal lost resources for:
      - exec sessions
      - PTY sessions
      - Python workers
      - MCP transports
      - browser sessions
      - browser script runs
- [x] Runtime rollback closes or marks child edges after rollback point.
- [x] Runtime rollback updates projection and journal consistently.
- [x] Replay compaction must not consume mailbox or mutate child state.

## Work Package 14: Deletions And Demotions

- [x] `LiveAgentExecutor` is a deprecated compatibility alias for
      `RuntimeAgentExecutor`.
- [x] `RuntimeAgentExecutor` no longer calls
      `run_session_with_config_with_cancel_and_runtime`.
- [x] `run_session_with_config*` is deleted, private test-only, or a wrapper over
      `RuntimeHandle::run_agent`.
- [x] `StoreTurnState` is deleted, private test-only, or replay-only.
- [x] `StoreNotificationWatcher` is not used by live subagent wait/wakeup.
- [x] `agent_messages` is not used as live mailbox state.
- [x] Provider-created global exec manager is not the normal live path.
- [x] Browser process-wide session/script statics are demoted to compatibility
      wrapper paths and are not used by normal runtime-created browser backends.
- [x] TUI child OS-thread launcher is gone as a separate live authority.
- [x] CLI child Store-first launcher is gone as a separate live authority.
- [x] Direct Store polling is not used as a wakeup/scheduling primitive.

## Work Package 15: Verification

- [x] `cargo fmt --check`
- [x] `cargo test`
- [x] `uv run --with pytest python -m pytest -q`
- [x] `scripts/verify-terminal-ui.sh`
- [x] Inspect `/tmp/but-design-loop/` dumps and tmux captures.

Runtime tests:

- [x] Runtime creates root agent and journals session.
- [x] Runtime starts and completes fake agent.
- [x] Runtime cancellation cancels active turn.
- [x] Journal replay hydrates completed session.
- [x] Journal replay hydrates pending mailbox.
- [x] Journal replay marks stale live resources lost/orphaned.
- [x] Event bus delivers ordered live events.
- [x] Projection serializes snapshot before live events.
- [x] Final projected event arrives before request resolves.
- [x] Critical journal failure prevents visible success/wakeup.

Subagent tests:

- [x] Spawn creates child `AgentThread`.
- [x] Strict capacity rejects immediately.
- [x] Explicit queue mode queues visibly.
- [x] Child completion enqueues non-triggering parent mailbox item.
- [x] `wait_agent` returns immediately when mail is pending.
- [x] `wait_agent` wakes on mailbox seq change.
- [x] `wait_agent` timeout does not hide child status.
- [x] Parent idle wake schedules continuation only for `trigger_turn=true`.
- [x] `close_agent` cancels descendants.
- [x] `resume_agent` materializes durable child metadata without stale handles.
- [x] Full-history fork rejects model/type/reasoning overrides.

Tool and browser tests:

- [x] `exec_command` process survives follow-up in same agent.
- [x] `write_stdin` cannot address another agent's process by default.
- [x] Agent close kills remaining exec processes.
- [x] Foreground exec is cancelled by turn cancel.
- [x] Background exec survives turn cancel and remains pollable.
- [x] Python worker persists across follow-up.
- [x] Python worker closes on agent close.
- [x] Browser script runs are scoped to `BrowserHandle`.
- [x] Browser script cancel preserves partial output/artifacts.
- [x] Same browser concurrent agents fail fast.
- [x] Different browsers run in parallel.
- [x] MCP manager shuts down on agent/runtime close.

TUI tests:

- [x] Subagents stay visible while running.
- [x] Queued subagents stay visible.
- [x] Wait timeout does not blank live subagent panel.
- [x] Child completion appears as status/mail without hiding child.
- [x] Trigger-turn follow-up wakes parent and displays continuation.
- [x] Rollback updates live projection and history.
- [x] Auth-nudge resume routes through runtime.
- [x] History overlay still reads old SQLite sessions.
- [x] Terminal smoke has no stale redraws or leaked escape sequences.

SDK tests:

- [x] `Agent(task, llm).run()`.
- [x] `Agent.add_new_task()` preserves memory.
- [x] Two agents with different browsers run concurrently.
- [x] Two agents with same browser fail fast.
- [x] `agent.stop()` cancels Rust run.
- [x] asyncio cancellation cancels Rust run.
- [x] `history.final_result()`.
- [x] structured output validation.

Live smoke tests:

- [x] `spin up 2 subagents ask them whats the capital of france and compare answers`
- [x] `spin up 3 subagents research this codebase`
- [x] spawn 3 subagents under default cap without hitting the false off-by-one limit
- [x] spawn many subagents above concurrency limit
- [x] ask parent what happened after children complete
- [x] cancel parent with children running
- [x] run `browser_script` while subagents run
- [x] run background `exec_command`, then poll/interact with `write_stdin`

Verification evidence:

- `cargo fmt --check` passed.
- `cargo test` passed.
- `uv run --with pytest python -m pytest -q` passed with `34 passed`.
- `/opt/homebrew/bin/timeout -k 15s 1200s scripts/verify-terminal-ui.sh`
  passed on 2026-06-04. Artifact inspection covered `/tmp/but-design-loop/`
  deterministic dumps and `tui-terminal-smoke-*.txt`; no leaked escape
  sequences, stale redraw signatures, duplicate app chrome, or panic text were
  found.
- Live GPT-5.5 two-subagent capital smoke passed:
  `/tmp/but-live-runtime-subagents.rZBMCP`, root
  `2ea16188-1f4d-4dd1-a291-c6568fa69fee`, `agent_messages=0`, mailbox
  enqueue/deliver/consume events present.
- Live GPT-5.5 three-subagent codebase research smoke passed:
  `/tmp/but-live-runtime-research-fixed.Ss0NHw`, root
  `4cd84f05-7258-46b5-a983-7981d7acec10`, three spawns, no false
  off-by-one capacity rejection, `agent_messages=0`.
- Live GPT-5.5 over-capacity smoke passed:
  `/tmp/but-live-runtime-overcap.RZbsp8`, root
  `1af30bda-c016-4adb-8fa4-5974d654822c`, three successful children, three
  immediate capacity rejections, `agent_messages=0`.
- Live GPT-5.5 parent-cancel-with-children smoke passed:
  `/tmp/but-live-runtime-cancel-final.wWpbgk`, root
  `30ad16d8-5e66-4ddd-ad7a-3494a015c77d`, all descendants ended
  `cancelled`, cancellation request/abort/cancelled events were journaled for
  root and children, and no child success mailbox was projected after cancel.
- Live GPT-5.5 browser-script-plus-subagents smoke passed:
  `/tmp/but-live-runtime-browser-script-nocdp.cdpzvk`, root
  `eab5609f-83cf-4ac0-aefa-aa3c549df3f6`, `browser_script.started`,
  `browser_script.completed`, `browser.claimed`, and `browser.released`
  journaled with two subagent completions and `agent_messages=0`.
- Live GPT-5.5 background exec/write smoke passed:
  `/tmp/but-live-runtime-exec-stdin.norB3L`, root
  `e0dfdff0-b15c-4872-b3e5-9ae64abbf43a`, `exec_command.begin`,
  `command.waiting`, `terminal.interaction`, and `exec_command.end` journaled;
  `write_stdin` delivered `hello\n` and final output contained `got:hello`.

## Final Definition Of Done

- [x] The ASCII target architecture above matches the code.
- [x] `BrowserUseRuntime` owns live turn execution.
- [x] `StoreTurnState` is not the live default.
- [x] `run_session_with_config*` is not a parallel live authority.
- [x] Store-backed subagent wait/mailbox scheduling is unreachable for live runs.
- [x] TUI active state comes from runtime projection.
- [x] SDK run/stream/cancel/follow-up goes through runtime.
- [x] Browser ownership and script runs are physically owned by
      `BrowserManager` end to end.
- [x] Tool resources are owned by `AgentThread.ToolResourceBag`.
- [x] Replay restores durable state and marks stale live resources lost.
- [x] Barrier failure tests prove SQLite never decides wakeups but still protects
      accepted facts.
- [x] All verification commands and live smokes above pass.

## Proposed `set_goal`

```text
Implement docs/agent-design/live-runtime-inversion-checklist.md end to end. Make BrowserUseRuntime the sole live authority by replacing LiveAgentExecutor -> run_session_with_config_with_cancel_and_runtime and StoreTurnState with RuntimeHandle::run_agent, RuntimeTurnDriver, LiveTurnState, AgentThread.ToolResourceBag, BrowserHandle/BrowserManager ownership, runtime projection, journal barrier semantics, replay/lost-resource materialization, and runtime-only subagent/mailbox/wait/followup/close behavior. Delete or demote all store-first live paths, including StoreNotificationWatcher live wait, agent_messages live mailbox behavior, TUI/CLI child store launchers, provider-created global resource ownership, and direct Store polling as wakeup/scheduling. Keep SQLite as the complete debug journal and replay source only. Do not mark complete until every checkbox in the final definition of done is satisfied, scripts/verify-terminal-ui.sh passes, and the listed live GPT-5.5 subagent/tool/browser smoke tests pass with SQLite inspection confirming no duplicate or store-owned live transitions.
```
