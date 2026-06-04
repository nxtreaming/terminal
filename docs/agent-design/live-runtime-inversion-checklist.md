# Live Runtime Inversion Implementation Checklist

This is the implementation checklist for finishing the original live-runtime
rewrite. The current branch is a runtime-backed hybrid. This checklist is for
the remaining inversion: the runtime owns live agent execution, while SQLite is
only persistence, replay, and postmortem debugging.

## Current Hybrid

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

- [ ] Runtime is the only live authority for turns, input, mailbox wakeups,
      subagent state, browser leases, cancellation, and tool resources.
- [ ] SQLite is a write-through journal and replay source only.
- [ ] No live path decides work by polling `Store` rows or
      `StoreNotificationWatcher`.
- [ ] No accepted user input, mailbox wakeup, child completion, browser claim,
      or terminal status is visible before its barrier journal write succeeds.
- [ ] Old public APIs may remain only as wrappers over runtime APIs.
- [ ] Unsupported SDK parity fails loudly.
- [ ] TUI active sessions render from runtime projection, not store refreshes.
- [ ] History, export, postmortem, and debug views can still read SQLite.

## Work Package 0: Baseline And Guardrails

- [ ] Keep this work on `codex/live-runtime-rewrite` or rename to a clearly
      scoped runtime-inversion branch.
- [ ] Record the current hybrid seams:
      - `crates/browser-use-agent/src/live_executor.rs`
      - `crates/browser-use-agent/src/entrypoint/mod.rs::StoreTurnState`
      - `crates/browser-use-agent/src/tools/handlers/subagent.rs`
      - `crates/browser-use-cli/src/main.rs::spawn_cli_child_agent`
      - `crates/browser-use-tui/src/runtime.rs::spawn_tui_child_agent`
- [ ] Add compile-time or test guards that fail if new live code calls
      store-backed wait/mailbox scheduling.
- [ ] Keep the existing successful smoke prompt as a regression:
      `spin up 2 subagents ask them whats the capital of france and compare answers`.

## Work Package 1: Runtime API Schema

- [ ] Add `RunAgentRequest`:
      - `agent_id`
      - `run_id`
      - `provider_config`
      - `initial_input`
      - `browser_id`
      - `cwd`
      - `cancel_token`
      - `input_source`
      - `resume_mode`
- [ ] Add `RunAgentResponse`:
      - `agent_id`
      - `run_id`
      - `final_status`
      - `final_result`
      - `usage`
      - `terminal_event_seq`
- [ ] Add `RuntimeHandle::run_agent(request)`.
- [ ] Add `RuntimeHandle::submit_input(agent_id, input, trigger_turn)`.
- [ ] Add `RuntimeHandle::submit_followup(agent_id, input)`.
- [ ] Add `RuntimeHandle::snapshot_agent(agent_id)`.
- [ ] Add `RuntimeHandle::subscribe_agent_projection(agent_id)`.
- [ ] Make `RuntimeHandle::cancel_run`, `close_agent`, `wait_agent`,
      `send_agent_message`, and browser lease APIs use the same ids and
      lifecycle model.

## Work Package 2: AgentThread State

- [ ] Expand `AgentThread` with `LiveTurnState`.
- [ ] Add an in-memory input queue for accepted root input.
- [ ] Add an in-memory follow-up queue.
- [ ] Add mailbox delivery cursors:
      - last enqueued seq
      - last delivered seq
      - last consumed seq
      - current-turn vs next-turn delivery phase
- [ ] Add per-agent token usage and compaction window state.
- [ ] Add per-agent current run state:
      - idle
      - queued
      - running
      - cancelling
      - completed
      - failed
      - closed
- [ ] Add per-agent cancellation tree:
      - agent token
      - run token
      - turn token
      - tool tokens
- [ ] Add `ToolResourceBag` to `AgentThread`.
- [ ] Add runtime snapshots that include live state without querying SQLite.

## Work Package 3: RuntimeTurnDriver

- [ ] Extract the reusable model/tool loop from
      `run_session_with_config_with_cancel_and_runtime`.
- [ ] Create `RuntimeTurnDriver` that receives:
      - `AgentThread`
      - `LiveTurnState`
      - `ToolResourceBag`
      - `RuntimeEventSink`
      - `ProviderRunConfig`
- [ ] Make first sampling input come from runtime accepted input, not
      `Store::events_for_session`.
- [ ] Make follow-up sampling input come from runtime follow-up queue.
- [ ] Make mailbox input come from runtime mailbox delivery, not
      `agent_messages`.
- [ ] Preserve existing model/tool fusion behavior.
- [ ] Preserve existing compaction behavior, but move compaction state out of
      `StoreTurnState`.
- [ ] Preserve existing token accounting behavior, but source live token state
      from `LiveTurnState`.
- [ ] Journal model/tool events through runtime before publishing them.
- [ ] Flush best-effort deltas before terminal completion.
- [ ] Return terminal status through `RunAgentResponse`.

## Work Package 4: Replace StoreTurnState

- [ ] Implement `LiveTurnState: TurnState`.
- [ ] Port prompt reconstruction from Store-backed history into
      `JournalReader` replay helpers.
- [ ] Port pending input logic from `StoreTurnState` to `LiveTurnState`.
- [ ] Port mailbox current-turn/next-turn logic to runtime mailbox cursors.
- [ ] Port active follow-up drain logic to runtime follow-up queue.
- [ ] Port compaction checkpoint writes to `JournalSink`.
- [ ] Port token status recomputation to live state plus journal replay.
- [ ] Remove `StoreTurnState` from the normal live run path.
- [ ] Keep any remaining Store-based turn state only under explicit test or
      replay compatibility names.

## Work Package 5: Journal Barriers And Event Contract

- [ ] Enforce barrier-before-publish for:
      - session/agent create
      - accepted user input
      - mailbox enqueue
      - mailbox delivered
      - mailbox consumed
      - spawn edge open/close
      - browser claim/release
      - terminal agent status
      - cancellation request
- [ ] Emit `mailbox.delivered` when input becomes visible to a turn.
- [ ] Emit `mailbox.consumed` when the delivered item has been committed into
      prompt/history.
- [ ] Persist consumed seq so resume cannot redeliver old mail.
- [ ] Add negative tests where barrier append failure prevents:
      - accepted input success
      - mailbox wake
      - child completion success
      - browser claim success
      - final completion success
- [ ] Keep SQLite postmortem complete enough to explain every live transition.

## Work Package 6: Subagents And AgentControl

- [ ] Make `spawn_agent` a thin runtime call.
- [ ] Make `send_message` a thin runtime call.
- [ ] Make `followup_task` a thin runtime call.
- [ ] Make `wait_agent` a thin runtime call.
- [ ] Make `close_agent` a thin runtime call.
- [ ] Make `resume_agent` hydrate through runtime, not direct store scheduling.
- [ ] Delete or make unreachable store-backed live wait paths:
      - `StoreNotificationWatcher`
      - store-backed `wait_agent`
      - store-backed mailbox drain as live behavior
- [ ] Keep store-backed helpers only for replay/history/compat tests.
- [ ] Preserve Codex semantics:
      - child completion mail has `trigger_turn=false`
      - `wait_agent` wakes but does not return child content
      - follow-up/initial delegated mail may use `trigger_turn=true`
      - full-history fork inherits model/type/reasoning and rejects overrides
- [ ] Implement strict capacity as the default.
- [ ] Implement queued capacity only behind explicit config.
- [ ] Ensure completed children remain visible until explicitly closed.

## Work Package 7: ToolResourceBag

- [ ] Move `UnifiedExecManager` ownership into `AgentThread.ToolResourceBag`.
- [ ] Remove provider-created global exec manager as normal live path.
- [ ] Make `write_stdin` validate agent ownership of process ids.
- [ ] Move Python worker ownership into `ToolResourceBag`.
- [ ] Keep Python worker alive across follow-ups for the same agent.
- [ ] Close Python worker on agent close/runtime shutdown.
- [ ] Move MCP manager ownership into `ToolResourceBag`.
- [ ] Close MCP transports on agent close/runtime shutdown.
- [ ] Ensure tool handlers receive resources from runtime/tool context.
- [ ] Mark old resources lost/orphaned on crash/resume instead of pretending
      handles survived.

## Work Package 8: BrowserManager And BrowserHandle

- [ ] Extend `BrowserHandle` beyond metadata:
      - CDP/browser process/session state
      - script run registry
      - capture/artifact ownership
      - active agent lease
      - action serialization
- [ ] Make browser tools resolve a `BrowserHandle`, not only a `session_id`.
- [ ] Enforce one running agent per browser.
- [ ] Allow many browsers in parallel.
- [ ] Make `browser_script` start/observe/cancel operate through
      `BrowserHandle`.
- [ ] Journal browser claim/release/start/close as barrier events.
- [ ] Journal script output/completion/cancellation in order.
- [ ] Make SDK `Browser` map to `browser_id`.
- [ ] On crash/resume, mark old browser/script handles lost or orphaned.

## Work Package 9: Runtime Projection

- [x] Implement a real `RuntimeEventProjection` state machine, not just event
      wrapping.
- [ ] Projection includes:
      - [x] active agent status
      - [ ] live model/tool activity
        - [x] observed tool start/delta/completion state
        - [x] latest model stream/thinking deltas
        - [ ] active model request/retry/error lifecycle
      - [x] child agent statuses
      - [x] mailbox continuation state
      - [x] browser status
      - [x] token usage
      - [x] final result/failure
- [x] Projection guarantees snapshot-before-live-event ordering.
- [x] Projection sends final status before `run_agent` resolves.
- [ ] TUI consumes projection for active sessions.
- [x] SDK consumes projection for `stream()`.
- [x] Raw runtime event stream remains opt-in for debugging.

## Work Package 10: TUI Integration

- [ ] Replace active session rendering from Store cache with runtime projection.
- [ ] Keep SQLite reads for history/sidebar/resume only.
- [ ] Remove or demote `TUI_LIVE_EXECUTORS`.
- [ ] Remove `spawn_tui_child_agent` as a live launcher.
- [ ] Route root runs, followups, cancel, close, auth resume, and mailbox
      continuation through runtime APIs.
- [ ] Ensure running subagents stay visible during waits and timeouts.
- [ ] Ensure completed subagents stay visible until explicit close.
- [ ] Ensure wait timeout does not blank child panels.
- [ ] Ensure terminal output remains selectable plain text after completion.

## Work Package 11: CLI Integration

- [ ] Replace `run_session_via_engine*` with `RuntimeHandle::run_agent`.
- [ ] Remove `spawn_cli_child_agent` as a live launcher.
- [ ] Live commands use runtime:
      - run
      - followup
      - resume/live continuation
      - cancel/stop
      - close/send/wait/list subagents
      - browser_script
      - Python live execution
      - eval/dataset live execution
- [ ] Journal/history commands use Store/StateIndex:
      - show
      - history
      - export
      - inspect
      - cleanup
      - auth/config/profile commands not tied to live handles
- [ ] Cleanup commands must not kill unrelated live runtime resources.

## Work Package 12: SDK Integration

- [ ] Keep `browser-use-terminal sdk-server --transport stdio`.
- [ ] Make SDK `agent.run` call runtime `run_agent`.
- [ ] Add SDK event stream notifications from runtime projection.
- [ ] Add `Agent.stream()`.
- [ ] Add `Agent.stop()` cancellation through runtime.
- [ ] Add asyncio cancellation -> runtime cancellation.
- [ ] Add follow-up support through runtime input/follow-up queue.
- [ ] Add one-browser-one-running-agent enforcement.
- [ ] Add different-browser parallel run support.
- [ ] Add structured output validation.
- [ ] Remove SDK agent runs that scrape CLI output or call old store-first paths.

## Work Package 13: Replay, Resume, Rollback, And Crash Recovery

- [ ] Add runtime materializer from `JournalReader` and `StateIndex`.
- [ ] Hydrate:
      - root session metadata
      - child tree/spawn edges
      - pending mailbox items
      - consumed mailbox seq
      - goals/followups
      - config/model/provider state
      - durable artifacts
      - transcript/history
- [ ] Do not hydrate live process handles.
- [ ] Journal lost resources for:
      - exec sessions
      - PTY sessions
      - Python workers
      - MCP transports
      - browser sessions
      - browser script runs
- [ ] Runtime rollback closes or marks child edges after rollback point.
- [ ] Runtime rollback updates projection and journal consistently.
- [ ] Replay compaction must not consume mailbox or mutate child state.

## Work Package 14: Deletions And Demotions

- [ ] `LiveAgentExecutor` no longer calls
      `run_session_with_config_with_cancel_and_runtime`.
- [ ] `run_session_with_config*` is deleted, private test-only, or a wrapper over
      `RuntimeHandle::run_agent`.
- [ ] `StoreTurnState` is deleted, private test-only, or replay-only.
- [ ] `StoreNotificationWatcher` is not used by live subagent wait/wakeup.
- [ ] `agent_messages` is not used as live mailbox state.
- [ ] Provider-created global exec manager is not the normal live path.
- [ ] Browser process-wide session/script statics are only reachable through
      `BrowserManager`, if they remain internally.
- [ ] TUI child OS-thread launcher is gone as a separate live authority.
- [ ] CLI child Store-first launcher is gone as a separate live authority.
- [ ] Direct Store polling is not used as a wakeup/scheduling primitive.

## Work Package 15: Verification

- [ ] `cargo fmt --check`
- [ ] `cargo test`
- [ ] `uv run --with pytest python -m pytest -q`
- [ ] `scripts/verify-terminal-ui.sh`
- [ ] Inspect `/tmp/but-design-loop/` dumps and tmux captures.

Runtime tests:

- [ ] Runtime creates root agent and journals session.
- [ ] Runtime starts and completes fake agent.
- [ ] Runtime cancellation cancels active turn.
- [ ] Journal replay hydrates completed session.
- [ ] Journal replay hydrates pending mailbox.
- [ ] Journal replay marks stale live resources lost/orphaned.
- [ ] Event bus delivers ordered live events.
- [ ] Projection serializes snapshot before live events.
- [ ] Final projected event arrives before request resolves.
- [ ] Critical journal failure prevents visible success/wakeup.

Subagent tests:

- [ ] Spawn creates child `AgentThread`.
- [ ] Strict capacity rejects immediately.
- [ ] Explicit queue mode queues visibly.
- [ ] Child completion enqueues non-triggering parent mailbox item.
- [ ] `wait_agent` returns immediately when mail is pending.
- [ ] `wait_agent` wakes on mailbox seq change.
- [ ] `wait_agent` timeout does not hide child status.
- [ ] Parent idle wake schedules continuation only for `trigger_turn=true`.
- [ ] `close_agent` cancels descendants.
- [ ] `resume_agent` materializes durable child metadata without stale handles.
- [ ] Full-history fork rejects model/type/reasoning overrides.

Tool and browser tests:

- [ ] `exec_command` process survives follow-up in same agent.
- [ ] `write_stdin` cannot address another agent's process by default.
- [ ] Agent close kills remaining exec processes.
- [ ] Foreground exec is cancelled by turn cancel.
- [ ] Background exec survives turn cancel and remains pollable.
- [ ] Python worker persists across follow-up.
- [ ] Python worker closes on agent close.
- [ ] Browser script runs are scoped to `BrowserHandle`.
- [ ] Browser script cancel preserves partial output/artifacts.
- [ ] Same browser concurrent agents fail fast.
- [ ] Different browsers run in parallel.
- [ ] MCP manager shuts down on agent/runtime close.

TUI tests:

- [ ] Subagents stay visible while running.
- [ ] Queued subagents stay visible.
- [ ] Wait timeout does not blank live subagent panel.
- [ ] Child completion appears as status/mail without hiding child.
- [ ] Trigger-turn follow-up wakes parent and displays continuation.
- [ ] Rollback updates live projection and history.
- [ ] Auth-nudge resume routes through runtime.
- [ ] History overlay still reads old SQLite sessions.
- [ ] Terminal smoke has no stale redraws or leaked escape sequences.

SDK tests:

- [ ] `Agent(task, llm).run()`.
- [ ] `Agent.add_new_task()` preserves memory.
- [ ] Two agents with different browsers run concurrently.
- [ ] Two agents with same browser fail fast.
- [ ] `agent.stop()` cancels Rust run.
- [ ] asyncio cancellation cancels Rust run.
- [ ] `history.final_result()`.
- [ ] structured output validation.

Live smoke tests:

- [ ] `spin up 2 subagents ask them whats the capital of france and compare answers`
- [ ] `spin up 3 subagents research this codebase`
- [ ] spawn many subagents above concurrency limit
- [ ] ask parent what happened after children complete
- [ ] cancel parent with children running
- [ ] run `browser_script` while subagents run
- [ ] run background `exec_command`, then poll/interact with `write_stdin`

## Final Definition Of Done

- [ ] The ASCII target architecture above matches the code.
- [ ] `BrowserUseRuntime` owns live turn execution.
- [ ] `StoreTurnState` is not the live default.
- [ ] `run_session_with_config*` is not a parallel live authority.
- [ ] Store-backed subagent wait/mailbox scheduling is unreachable for live runs.
- [ ] TUI active state comes from runtime projection.
- [ ] SDK run/stream/cancel/follow-up goes through runtime.
- [ ] Browser ownership and script runs go through `BrowserManager`.
- [ ] Tool resources are owned by `AgentThread.ToolResourceBag`.
- [ ] Replay restores durable state and marks stale live resources lost.
- [ ] Barrier failure tests prove SQLite never decides wakeups but still protects
      accepted facts.
- [ ] All verification commands and live smokes above pass.

## Proposed `set_goal`

```text
Implement docs/agent-design/live-runtime-inversion-checklist.md end to end. Make BrowserUseRuntime the sole live authority by replacing LiveAgentExecutor -> run_session_with_config_with_cancel_and_runtime and StoreTurnState with RuntimeHandle::run_agent, RuntimeTurnDriver, LiveTurnState, AgentThread.ToolResourceBag, BrowserHandle/BrowserManager ownership, runtime projection, journal barrier semantics, replay/lost-resource materialization, and runtime-only subagent/mailbox/wait/followup/close behavior. Delete or demote all store-first live paths, including StoreNotificationWatcher live wait, agent_messages live mailbox behavior, TUI/CLI child store launchers, provider-created global resource ownership, and direct Store polling as wakeup/scheduling. Keep SQLite as the complete debug journal and replay source only. Do not mark complete until every checkbox in the final definition of done is satisfied, scripts/verify-terminal-ui.sh passes, and the listed live GPT-5.5 subagent/tool/browser smoke tests pass with SQLite inspection confirming no duplicate or store-owned live transitions.
```
