# Full Live Runtime Rewrite Plan

This is the one-shot architecture plan for replacing the current store-driven
runtime with a memory-first runtime while keeping SQLite as a complete external
debug journal.

The target is not a small mailbox fix. The target is a new runtime ownership
model that makes subagents, follow-ups, command execution, browser actions, TUI,
CLI, and future SDKs all use the same live control plane.

## Core Decision

SQLite remains essential, but it changes role.

```text
Current shape:

live behavior often inferred from SQLite rows
  sessions
  events
  agent_edges
  agent_messages
  StoreNotification / watcher / polling

Target shape:

memory decides what happens now
SQLite records exactly what happened
```

The invariant:

```text
Memory is the control plane.
SQLite is the write-through journal and replay source.
```

Every externally useful fact still lands in SQLite: model calls, tool calls,
subagent status, mailbox enqueue/delivery/consume, wait start/end/timeout,
browser claims, command output, Python artifacts, cancellations, final results,
and failures.

SQLite must not decide:

- which agent is currently running
- which agent should wake up
- which mailbox items are pending for live delivery
- which subagent should start next
- which browser is actively claimed
- which command/process/script handles are alive
- whether a parent continuation should be scheduled

## High-Level Target

```text
                         user surfaces

     TUI          CLI          Python SDK          future app/server
      |            |               |                       |
      +------------+---------------+-----------------------+
                               |
                               v
                         RuntimeHandle
                               |
                               v
                      BrowserUseRuntime
                               |
        +----------------------+----------------------+
        |                      |                      |
        v                      v                      v
   AgentManager          BrowserManager          RuntimeEventBus
        |                      |                      |
        v                      v                      v
   AgentThread           BrowserHandle     RuntimeEventProjection
        |
        +-- AgentControl
        |     +-- AgentRegistry
        |     +-- SubagentScheduler
        |     +-- MailboxRouter
        |
        +-- TurnDriver
        +-- ToolResourceBag
        +-- CancellationToken
        +-- InMemoryHistory
        |
        v
   LiveThreadPersistence + StateIndex + JournalSink
        |
        v
      SQLite / memory implementation
```

Codex equivalent mental model:

```text
Codex ThreadManager      -> BrowserUseRuntime / AgentManager
Codex CodexThread        -> AgentThread
Codex AgentControl       -> AgentControl
Codex Session mailbox    -> AgentMailbox
Codex ThreadStore        -> LiveThreadPersistence
Codex StateDb            -> StateIndex
Codex rollout recorder   -> JournalSink
Codex app-server events  -> RuntimeEventProjection + RuntimeEventBus
```

Browser Use extra pieces:

```text
BrowserManager
PythonWorkerManager
UnifiedExecManager ownership
BrowserScriptRun ownership
SQLite journal optimized for outside debugging
Python SDK stdio server
```

## Public Input Surfaces

The runtime must be designed around all current and near-future entrypoints, not
only the TUI.

```text
TUI
  interactive user input
  history/resume overlays
  cancel/stop/rollback/auth resume
  follow-up tasks
  subagent panels and mailbox continuation state

CLI
  one-shot run
  live resume/follow-up/stop
  eval and dataset execution
  browser-script commands
  Python tool commands
  profile/cookie/auth/config/history/export/cleanup commands

Python SDK
  long-lived Browser objects
  concurrent Agent.run calls
  stream/cancel/follow-up
  structured output

Future app/server surfaces
  web UI
  remote worker
  cloud runner
  external debuggers reading SQLite
```

Every live surface talks to `BrowserUseRuntime`. Every debugging/history surface
talks to `JournalReader`/`StateIndex`. No surface should secretly recreate live
agent behavior by reopening SQLite and polling rows.

## What Changes For `exec_command`, `browser_script`, Python, And MCP

These tools are already asynchronous data-plane resources. The rewrite should
not throw away their internal implementation. It should change ownership.

Current problem:

```text
tool call
  -> handler owns or discovers a process/browser/script manager by session_id
  -> events are emitted through Store/SharedStore
  -> cancellation is partly turn-local, partly global/static, partly DB status
```

Target:

```text
tool call
  -> ToolCtx includes AgentThread identity and live RuntimeHandle
  -> handler uses AgentThread.ToolResourceBag or BrowserHandle
  -> events go to RuntimeEventBus and JournalSink
  -> cancellation comes from the AgentThread/TurnDriver cancellation tree
```

Tool internals stay mostly intact:

- `exec_command` still uses `UnifiedExecManager`.
- `write_stdin` still talks to a live process session.
- `browser_script` still starts/observes/cancels script runs.
- Python still uses a persistent worker process.
- MCP still owns live server connections.

But their handles move under live runtime objects:

```text
AgentThread.ToolResourceBag
  +-- unified_exec: UnifiedExecManager
  +-- python_worker: Option<PythonWorkerHandle>
  +-- mcp_connections: McpConnectionManager
  +-- approval_cache
  +-- tool_search cache/state

BrowserHandle
  +-- cdp/session/managed browser state
  +-- browser_script_runs
  +-- active_agent_id
  +-- browser event stream
```

This is important for SDKs. A Python `Browser` object needs to map to a real
`BrowserHandle`, not to a session-id string that a static global may or may not
interpret correctly.

## Current Codebase Inventory

### Crates

```text
browser-use-agent          agent loop, tools, provider resolution, subagents
browser-use-browser        browser/CDP/session/script runtime
browser-use-cli            CLI entrypoint and history commands
browser-use-tui            terminal UI, state cache, runtime thread launcher
browser-use-store          SQLite schema and store API
browser-use-protocol       shared event/session/artifact types
browser-use-python-worker  persistent Python subprocess protocol
browser-use-llm            model schema/client plumbing
browser-use-providers      auth/provider helper code
```

### Current Live-Control Dependencies To Replace

| Area | Current files | Current live-state problem | Target |
| --- | --- | --- | --- |
| Run entrypoint | `crates/browser-use-agent/src/entrypoint/mod.rs` | `run_session_with_config*` takes `SharedStore` and `session_id`; `StoreTurnState` reads/drains live input from SQLite. | `RuntimeHandle.run_agent(agent_id, run_id)` owns live state; journal is injected separately. |
| Turn state | `entrypoint/mod.rs` `StoreTurnState` | Prompt input, followups, mailbox mail, cancellation probes, and compaction read DB during live turns. | `AgentThread.turn_state` is live memory; journal is append/replay only. |
| Cancellation | `entrypoint/mod.rs`, `browser-use-tui/src/runtime.rs` | DB status monitor and TUI `ACTIVE_AGENT_RUNS` static map. | Runtime cancellation tree: root agent token, turn token, child tokens, tool tokens. |
| Subagents | `tools/handlers/subagent.rs`, `subagents/manager.rs`, `subagents/mailbox.rs`, `subagents/parent_link.rs` | Store-backed spawn/wait/mailbox logic competes with in-memory manager logic. | `AgentControl` is the only subagent authority. Store code becomes journaling/replay. |
| TUI child runs | `browser-use-tui/src/runtime.rs` | Spawns OS threads that reopen the same state dir and race through SQLite. | TUI submits to shared `BrowserUseRuntime`; no per-child store-open thread launcher. |
| CLI child runs | `browser-use-cli/src/main.rs` | Similar store-first child creation and run path. | CLI uses same runtime API; history commands remain Store readers. |
| TUI projection | `browser-use-tui/src/main.rs`, `transcript.rs` | UI derives active/live state from store cache and event rows. | UI renders live `RuntimeEvent`s; hydrates past transcript from SQLite. |
| Store notifications | `browser-use-store/src/lib.rs`, `session/notifier.rs` | `StoreNotificationWatcher` used as a live wakeup primitive. | Store notifications can refresh history, not wake agents. Runtime uses channels/watch. |
| Command execution | `tools/handlers/shell.rs`, `tools/unified_exec.rs`, `entrypoint/provider.rs` | Unified exec managers are created in provider/registry/static maps keyed by session id. | Each `AgentThread` owns its `UnifiedExecManager`; cleanup on agent close. |
| Browser script | `tools/handlers/browser.rs`, `browser-use-browser/src/lib.rs` | Browser sessions and script runs are process-wide statics keyed by `session_id`. | `BrowserManager` owns `BrowserHandle`; script runs are attached to browser/agent handles. |
| Python tool | `tools/handlers/python.rs`, `browser-use-python-worker/src/lib.rs` | Worker is currently per run and drops at run end. | Worker is a resource under `AgentThread`, persistent across follow-ups until agent close. |
| MCP | `mcp/manager.rs`, `mcp/stdio.rs`, `tools/handlers/mcp.rs` | Live connections are tool/provider scoped. | Runtime owns per-agent or per-profile MCP managers with explicit shutdown. |
| Goals/followups | `tools/handlers/goal.rs`, `entrypoint/mod.rs`, `tui/main.rs` | Goal/followup state is inferred from events. | Live goal and followup queues under `AgentThread`; events still journaled. |
| SDK | `python/llm_browser_worker/rust_cli.py` | Python launches Rust CLI/TUI binaries; no runtime protocol. | Python SDK talks to a long-lived stdio JSON-RPC server backed by `BrowserUseRuntime`. |

## Current Data Flow That Must Be Removed

```text
TUI/CLI
  -> Store::create_session()
  -> Store::append_event("session.input")
  -> run_session_with_config(SharedStore, session_id, config)
       -> StoreTurnState loads events from Store
       -> provider builds tool registry
       -> tools append/read Store
       -> subagents create child sessions in Store
       -> wait_agent waits on StoreNotificationWatcher
       -> child OS thread opens Store again
       -> parent resumes if DB rows imply pending mail
```

This makes failure modes hard to eliminate because the runtime and DB are both
trying to be the source of truth.

## Target Data Flow

```text
TUI/CLI/SDK
  -> runtime.create_agent(task, config, browser_id)
  -> runtime.start_run(agent_id)
       -> AgentThread becomes Running
       -> RuntimeEventBus emits agent.started
       -> JournalSink appends session.created / session.input / agent.started
       -> TurnDriver samples model
       -> ToolDispatcher uses AgentThread.ToolResourceBag
       -> subagents use AgentControl
       -> browser tools use BrowserManager
       -> all events go live first, then journal
  -> runtime returns AgentHistory snapshot
```

Subagent completion:

```text
child AgentThread completes
  -> status = Completed
  -> RuntimeEvent agent.completed
  -> JournalSink agent.completed
  -> parent.mailbox.push(SubagentNotification, trigger_turn=false)
  -> parent.mailbox.seq += 1
  -> RuntimeEvent mailbox.enqueued
  -> JournalSink agent_messages/mailbox.enqueued
  -> if message has trigger_turn=true and parent idle:
         Runtime schedules parent continuation
```

Important Codex-aligned rule:

```text
child completion by itself does not automatically start a parent turn
```

The completion notification is mail. It becomes model input on the next parent
turn, or immediately only when a separate explicit input/follow-up message has
`trigger_turn=true`. This avoids the failure mode where invisible background
events unexpectedly pull the parent back into a model call.

If Browser Use later wants a selected idle parent to auto-resume when child
results arrive, that is a deliberate non-Codex product mode. It needs a separate
runtime flag, visible UI state, and tests proving it cannot surprise unrelated
sessions.

`wait_agent`:

```text
wait_agent(parent, target_agent_or_path)
  -> subscribe parent.mailbox.seq
  -> if parent.mailbox.has_pending_completion_for(target): return completed
  -> wait for seq.changed() until deadline
  -> return { completed | timed_out | cancelled }
  -> child content is delivered through mailbox input, not tool output
```

`wait_agent` details:

- timeout values are validated and clamped by runtime config
- timeout never closes, hides, or consumes the child agent
- timed-out children stay visible in TUI/SDK/CLI projections
- a later completion still enqueues mail and wakes any future waiter
- targeted waits only complete for the requested child/path
- explicit any-agent waits can complete on the next matching child completion
- wait observes mailbox sequence numbers, not SQLite notifications
- v2 semantics are the default target; any v1 compatibility path must be
  isolated behind a compatibility adapter and tested separately

## New Runtime Types

### `BrowserUseRuntime`

Owns every live object.

```rust
pub struct BrowserUseRuntime {
    agents: AgentManager,
    browsers: BrowserManager,
    events: RuntimeEventBus,
    journal: Arc<dyn JournalSink>,
    config: RuntimeConfig,
}
```

Responsibilities:

- create, resume, start, cancel, and close agents
- create, start, claim, release, and close browsers
- own root-tree `AgentControl` instances
- expose live event streams to TUI/SDK
- write-through every transition to SQLite
- flush journal on shutdown and explicit SDK calls

Process-level services owned or borrowed here:

- provider/model registry and auth resolution
- environment/profile manager
- skills/plugins registry
- shared MCP manager factory
- `LiveThreadPersistence`
- `StateIndex`
- `JournalSink`
- thread-created/status broadcast
- runtime config and safety policy

Browser Use does not need to copy Codex's service names exactly, but it should
preserve this ownership shape: process-level dependencies are injected once,
then `AgentThread` gets scoped handles. Tool handlers should not rediscover
auth, config, MCP, or store state on every provider call.

### `AgentManager`

```rust
pub struct AgentManager {
    threads: RwLock<HashMap<AgentId, Arc<AgentThread>>>,
    session_to_agent: RwLock<HashMap<SessionId, AgentId>>,
    roots: RwLock<HashMap<RootId, AgentControl>>,
}
```

Responsibilities:

- keep live `AgentThread` handles
- enforce lifecycle transitions
- schedule parent continuations
- expose snapshots for UI and SDK
- load old sessions through `JournalReader` only during resume

### `AgentThread`

```rust
pub struct AgentThread {
    ids: AgentIds,
    metadata: AgentMetadata,
    status: watch::Sender<AgentStatus>,
    turn: TurnDriver,
    history: InMemoryHistory,
    mailbox: AgentMailbox,
    resources: ToolResourceBag,
    cancel: CancellationToken,
    browser_id: Option<BrowserId>,
}
```

Responsibilities:

- hold authoritative live conversation state
- hold active turn/cancellation state
- hold per-agent tool resources
- receive mailbox and follow-up input
- emit live events
- journal every significant transition

### `AgentControl`

One control object per root tree, shared by every descendant.

```rust
pub struct AgentControl {
    root_id: RootId,
    registry: AgentRegistry,
    scheduler: SubagentScheduler,
    mailbox: MailboxRouter,
    runtime: Weak<BrowserUseRuntimeInner>,
}
```

Responsibilities:

- `spawn_agent`
- `send_input`
- `followup_task`
- `wait_agent`
- `list_agents`
- `close_agent`
- `resume_agent`
- maintain root-tree path/name registry
- enforce max depth and max running subagents
- reserve and release spawn slots
- persist spawn edges through `StateIndex`
- inherit shell/environment snapshot and execution policy
- materialize fork history and filter forked rollout items
- inject completion notifications into parent mailbox
- close descendants when a parent/root closes
- resume open descendants when a root tree resumes

### `SubagentScheduler`

```rust
pub struct SubagentScheduler {
    max_open_spawned_agents: usize,
    open_spawned_agents: HashSet<AgentId>,
    queued: Option<VecDeque<QueuedSpawn>>,
}
```

Codex-aligned default:

```text
strict capacity mode:
  cap includes the root thread
  spawned-agent capacity = max_concurrent_threads_per_session - 1
  capacity counts open spawned agents, not only currently running agents
  completed-but-open agents still hold a slot
  slot releases on close/shutdown
  over-capacity spawn returns AgentLimitReached immediately
```

Optional Browser Use extension:

```text
queued capacity mode:
  over-capacity spawn creates visible Queued child
  queued child starts when an open child is closed or runtime policy permits
  this mode must be explicit, not the Codex-parity default
```

This preserves 1:1 Codex heuristics by default while still leaving a deliberate
path for "many many subagents" later. Queueing is a product feature, not a hidden
implementation detail.

### `AgentMailbox`

```rust
pub struct AgentMailbox {
    seq_tx: watch::Sender<u64>,
    queue: Mutex<VecDeque<MailboxItem>>,
}
```

Required semantics:

- `enqueue` increments `seq`
- `wait_agent` waits on `seq.changed()`
- pending mail is delivered to model input, not returned directly by wait
- delivery and consumption are journaled
- `trigger_turn=true` can schedule a parent continuation
- child completion mail uses `trigger_turn=false`
- mailbox survives live runtime for session lifetime
- SQLite can hydrate pending mail on resume

Delivery phase:

```rust
pub enum MailboxDeliveryPhase {
    CurrentTurn,
    NextTurn,
}
```

Rules:

- mail arriving before model sampling starts can be delivered in the current turn
- mail arriving while a turn is already producing visible output is buffered for
  the next turn unless it is explicit interrupting user input
- late mail after final output does not mutate the already-displayed answer
- steer/follow-up/tool activity can reopen the agent and deliver buffered mail
- every item has a monotonically increasing mailbox sequence number
- `wait_agent` waits on mailbox sequence changes, not on child status polling
- `wait_agent` matches target child/path metadata on mailbox items
- consumed sequence numbers are persisted so resume does not redeliver mail

### `BrowserManager`

```rust
pub struct BrowserManager {
    browsers: RwLock<HashMap<BrowserId, Arc<BrowserHandle>>>,
}

pub struct BrowserHandle {
    id: BrowserId,
    config: BrowserConfig,
    active_agent_id: Mutex<Option<AgentId>>,
    session: Mutex<BrowserSessionState>,
    script_runs: Mutex<HashMap<BrowserScriptRunId, BrowserScriptRun>>,
}
```

Responsibilities:

- one running agent per browser
- many browsers in parallel
- browser lifecycle independent from agent lifecycle when `keep_alive`
- browser script run handles live under the browser
- browser events go to runtime event bus and SQLite
- SDK `Browser` maps to this object

## Store, Index, And Journal Design

Keep `browser-use-store`, but split the concepts that are currently collapsed
into one SQLite-backed object.

```text
LiveThreadPersistence
  create_thread(...)
  load_thread(...)
  append_thread_item(...)
  flush_thread(...)
  shutdown_thread(...)
  list_threads(...)

StateIndex
  upsert_thread_metadata(...)
  upsert_dynamic_tool_metadata(...)
  upsert_memory_metadata(...)
  open_spawn_edge(parent, child, path, nickname, role)
  close_spawn_edge(parent, child)
  list_children(parent)
  list_descendants(root)
  mark_archived(thread)
  attach_feedback(thread, feedback)

JournalSink
  append_runtime_event(event)
  append_session_event(session_id, type, payload)
  append_mailbox_event(...)
  append_artifact(...)
  flush()

JournalReader
  load_session(session_id)
  list_sessions()
  events_for_session(session_id)
  load_agent_tree(root)
  load_pending_mail(session_id)
  load_history_for_fork(...)
```

SQLite can implement all three interfaces, but they are separate runtime
boundaries:

```text
LiveThreadPersistence is Codex ThreadStore-like:
  durable thread items and resume materialization

StateIndex is Codex StateDb-like:
  metadata, spawn edges, list/search, archive, feedback

JournalSink is Browser Use's debug superpower:
  complete event log for postmortems and external analysis
```

Tests and SDK memory-only mode can use memory implementations for all three.
Production TUI/CLI can use SQLite implementations for all three. The key rule is
that SQLite persistence may record and hydrate state, but it never schedules
live work by itself.

Journal durability:

```rust
pub enum Durability {
    Barrier,
    BestEffort,
}
```

Barrier facts must survive before user-visible success:

- agent/session create
- user input accepted
- mailbox enqueue/deliver/consume
- spawn edge open/close
- terminal agent statuses
- cancellation requests
- browser claims/releases
- final result/failure/cancellation

Best-effort facts can stream through the journal writer:

- model text deltas
- command/browser/Python/MCP output deltas
- transient progress updates

Journal rules:

- validate live state before appending
- append `Barrier` facts before publishing success or waking waiters
- append `BestEffort` facts through the ordered journal writer
- journal writer preserves per-session ordering
- buffered deltas must flush before final completion/cancellation is acknowledged
- shutdown calls `flush`
- crash recovery replays SQLite into fresh `AgentThread` objects and recreates or
  marks live resources lost; it does not resurrect old process handles

Do not use SQLite locks as runtime locks.

Mailbox enqueue order:

```text
validate live state
append mailbox.enqueued / agent_messages as Barrier
push into AgentMailbox memory queue
publish RuntimeEvent
wake waiters
schedule parent only if trigger_turn=true
```

This keeps memory as the live coordinator while making SQLite complete enough
for replay and debugging.

Spawn edge rules:

- edges are directional parent -> child
- edge state is `Open` or `Closed`
- completed children can remain `Open` and visible until explicitly closed
- root close closes all descendant edges and shuts down open descendants
- resume rehydrates open descendants and their mailbox state
- archive/export/history use `StateIndex`, not live runtime maps

## SQLite Event Contract

Add or standardize events so postmortems are straightforward:

```text
runtime.started
runtime.shutdown
agent.created
agent.started
agent.queued
agent.resumed
agent.completed
agent.failed
agent.cancel_requested
agent.cancelled
agent.closed
agent.wake_requested
agent.continuation_started
agent.turn.started
agent.turn.completed
agent.turn.aborted
subagent.spawn_requested
subagent.spawn_started
subagent.spawn_queued
subagent.spawn_rejected
subagent.spawn_completed
mailbox.enqueued
mailbox.delivered
mailbox.consumed
wait_agent.started
wait_agent.completed
wait_agent.timed_out
browser.created
browser.started
browser.claimed
browser.released
browser.closed
browser.script.started
browser.script.output_delta
browser.script.completed
browser.script.cancelled
exec_command.begin
exec_command.output_delta
exec_command.end
python.started
python.output_delta
python.completed
mcp.connected
mcp.tool.started
mcp.tool.completed
```

Existing event names can be preserved where the UI already depends on them, but
the runtime should own a typed `RuntimeEvent` enum and map it to existing
SQLite names through one adapter.

### Event Compatibility Matrix

The runtime event enum must cover the current observable event surface before
old live paths are removed.

| Current event family | Runtime event family | Notes |
| --- | --- | --- |
| `tool.output_delta` | `tool.output_delta` | Preserve streaming text deltas and order. |
| `tool.output` | `tool.completed` | Keep full final output for postmortem replay. |
| `tool.failed` | `tool.failed` | Include cancellation vs error reason. |
| `tool.image` | `artifact.created` + `tool.output_delta` | Images remain artifacts with stable references. |
| `artifact.created` | `artifact.created` | Artifact IDs must be ordered with producing tool events. |
| `command.waiting` | `command.waiting` | Required for long-running/background commands. |
| `terminal.interaction` | `command.input` | Used by `write_stdin`/PTY interaction replay. |
| browser capture events | `browser.capture.*` | Keep capture curation and screenshot/artifact links. |
| browser script deltas | `browser.script.output_delta` | Partial output must survive cancellation. |
| subagent rows/events | `agent.*`, `subagent.*`, `mailbox.*` | Runtime owns status; SQLite records transitions. |
| provider/model deltas | `model.*` | Needed for SDK streaming and TUI transcript. |
| MCP tool events | `mcp.*`, `tool.*` | Preserve server/tool identity and streamed output. |
| goal/follow-up events | `goal.*`, `input.*` | Needed for continuation and resume semantics. |

Artifact sequencing rules:

- every artifact records the producing `agent_id`, `run_id`, `turn_id`,
  `tool_call_id`, and monotonic event sequence
- artifact creation is emitted before any event that references the artifact
- cancelled browser/Python/shell work still journals partial artifacts
- replay can rebuild a transcript without reading live runtime state

### App/SDK Stream Ordering

Codex does not expose raw internal events directly to every app surface. It has
an app-server projection and listener ordering barrier. Browser Use needs the
same shape.

```text
RuntimeEventBus
  -> RuntimeEventProjection
       thread/status/changed
       turn/started
       turn/completed
       item/started
       item/completed
       agentMessage/delta
       command/output_delta
       mcp/output_delta
       typed CollabAgentToolCall-like tool updates
  -> per-surface subscription
       TUI
       SDK
       future app-server
```

Ordering guarantees:

- history snapshot and live events for a connection are serialized through one
  listener task
- a resume response cannot race ahead of already-emitted live events
- pending request resolution happens after final projected events for that turn
- raw events can be exposed for debugging, but UI/SDK default to projections
- idle runtime objects can unload only after all subscribed surfaces have seen
  final status or explicitly detached

### Journal Failure Policy

The journal cannot be allowed to silently eat important state transitions.

```text
critical events:
  agent.created/started/completed/failed/cancelled/closed
  subagent.spawn_started/spawn_rejected/spawn_completed
  mailbox.enqueued/delivered/consumed
  browser.claimed/released/closed
  artifact.created

best-effort buffered events:
  high-volume text/model/tool output deltas
  browser script stdout/stderr chunks
  command stdout/stderr chunks
```

Rules:

- critical event append failures surface as runtime errors
- buffered delta failures mark the run degraded and are reported in final status
- shutdown waits for a bounded flush
- SDK memory mode can disable SQLite, but not event ordering or live state rules
- TUI/CLI SQLite mode should preserve complete postmortem history by default

## Files To Remove Or Demote

Remove as live authority:

- `SharedStore` dependencies from tool handlers and live turn state.
- `StoreTurnState` as the default live `TurnState`.
- `spawn_store_cancel_monitor`.
- `StoreNotificationWatcher` usage in subagent wait/wakeup.
- TUI `ACTIVE_AGENT_RUNS`.
- TUI child-agent OS thread spawning that reopens the state dir.
- CLI child-agent spawning that constructs child live behavior through Store.
- `agent_messages` drain/update as live mailbox behavior.
- provider-time creation of tool resources that should be `AgentThread` owned.
- process-wide unified-exec cleanup maps keyed by session id.
- browser-use-browser process-wide session/script statics as public live state.
- event/store sink append paths that decide live control state.
- persistence helpers that both reconstruct and schedule work.
- config override paths that mutate active runtime behavior through Store rows.

Keep or demote:

- `browser-use-store::Store` for history, settings, artifacts, replay, and debug.
- existing SQLite tables, with added runtime/journal events where useful.
- pure reconstruction functions for replay/fork, adapted to `JournalReader`.
- tool schemas and most handler internals.
- `TaskDriver` and cancellation primitives, moved under `AgentThread`.
- `ToolOrchestrator`, approval, sandbox, guardian logic.
- `events/store_sink.rs` as a SQLite journal adapter, not a live event bus.
- `infra/persistence.rs` as replay/export infrastructure, not scheduling logic.
- `config_overrides.rs` as config hydration, not active control state.
- `session/reconstruct.rs`, `session/resume.rs`, and `session/rollback.rs` as
  replay/materialization helpers behind runtime APIs.
- `compact` logic, but driven from `AgentThread.history` rather than ad hoc
  event reads inside active turns.

## Rewiring By Component

### Agent Entry Point

Current:

```rust
run_session_with_config_with_cancel(
    store: SharedStore,
    session_id: &str,
    config: ProviderRunConfig,
    cancel: CancellationToken,
)
```

Target:

```rust
runtime.start_run(StartRunRequest {
    agent_id,
    run_id,
    config,
    input,
    browser_id,
})
```

The old function should become a compatibility wrapper:

```text
open/create runtime
create/resume agent from session id
start run
return session id
```

Long term, CLI/TUI/SDK should not call the old function.

### Turn State

Current `StoreTurnState` responsibilities split into:

```text
AgentThread.history
  prompt reconstruction from live items
  assistant/tool recording
  compaction state

AgentThread.input_queue
  user input
  followups
  mailbox delivery
  deferred next-turn delivery

JournalReader
  replay only: resume, fork, rollback
```

No live turn should call `events_for_session` to decide if there is pending
input.

### Fork, Resume, Rollback, And Compaction

These are not optional side paths. They define what a thread means after the
runtime stops and starts again.

Target ownership:

```text
runtime.resume_agent(session_id)
  -> JournalReader loads durable thread items
  -> StateIndex loads open spawn edges and metadata
  -> AgentManager materializes AgentThread objects
  -> AgentMailbox restores pending/unconsumed mail
  -> RuntimeEventProjection emits coherent status snapshots
```

Crash/resume resource policy:

```text
replay restores:
  transcript/history
  pending mailbox rows and consumed sequence markers
  agent tree and spawn edges
  goals and follow-up queues
  config/model/profile metadata
  durable artifacts and browser captures

replay does not restore:
  exec_command process handles
  PTY sessions
  Python worker namespace/process
  browser script subprocesses
  browser leases/action locks
  MCP transports and pending calls
  mailbox receiver tasks/watch subscriptions
```

On resume, live resources are either recreated from durable config or marked
lost/orphaned with explicit journal events. The runtime must not show an old
process, browser lease, script run, Python namespace, or MCP call as live unless
it has a fresh handle in the new process.

Fork rules:

- support `fork_turns = none | all | N`
- full-history fork inherits parent agent type, model, and reasoning effort
- full-history fork rejects overrides for agent type/model/reasoning effort
- forked rollout items are filtered so child-visible history matches Codex
- trigger-turn inter-agent messages count as turn boundaries
- inherited shell/environment snapshot and exec policy are explicit metadata
- old `MultiAgentV2` guidance that says to override inherited properties must
  be removed or made conditional on non-full-history fork mode

Rollback rules:

- rollback is requested through runtime, not by deleting SQLite rows directly
- rollback creates a new live history view and journals the rollback transition
- child edges and mailbox items after the rollback point are closed or marked
  unreachable according to explicit policy
- TUI rollback UI refreshes from runtime projection and then SQLite history

Compaction rules:

- compaction reads from `AgentThread.history` while live
- replay compaction reads from `JournalReader`
- compaction output is journaled as a history item/artifact
- compaction must not consume mailbox items or change child edge state

### Tool Context

Current:

```rust
pub struct ToolCtx {
    call_id: String,
    tool_name: String,
    cwd: PathBuf,
    artifact_root: PathBuf,
}
```

Target:

```rust
pub struct ToolCtx {
    call_id: String,
    tool_name: String,
    cwd: PathBuf,
    artifact_root: PathBuf,
    agent_id: AgentId,
    session_id: SessionId,
    run_id: RunId,
    browser_id: Option<BrowserId>,
    runtime: RuntimeHandle,
}
```

Handlers can still be unit-tested with a fake `RuntimeHandle`.

### Tool Registry

Current `default_registry` constructs some resources itself, including a fresh
`UnifiedExecManager`.

Target:

```text
AgentThread builds ToolResourceBag
ToolRegistry receives handlers backed by that bag
Provider resolution receives ToolRegistry or ToolDispatcher from AgentThread
```

`ToolOrchestrator` remains useful. The change is resource ownership, not the
approval/sandbox algorithm.

### `exec_command` And `write_stdin`

Keep:

- `UnifiedExecManager`
- process snapshots
- PTY support
- `write_stdin`
- output delta events
- cancellation support

Change:

- manager lives on `AgentThread.ToolResourceBag`
- foreground shell command uses current turn cancellation
- background `exec_command` survives across turns in the same agent
- `write_stdin` only sees sessions for that agent unless explicitly shared
- agent close cancels/kills remaining processes
- runtime journals `exec_command.*` events

Cancellation/resource policy:

| Resource | Turn cancel | Agent close | Runtime shutdown | Notes |
| --- | --- | --- | --- | --- |
| foreground `exec_command` | kill/cancel process | kill process | kill process | Final event says cancelled vs failed. |
| background `exec_command` | survives current turn | kill process | kill process | Snapshot remains pollable in same agent until close. |
| `write_stdin` | no independent cancel token | disabled after process kill | disabled | Writes only to processes owned by that agent unless explicitly shared. |
| PTY session | survives if background | close PTY | close PTY | Journal terminal input/output ordering. |
| output delta stream | stop after process final event | flush buffered deltas | bounded flush | Partial output remains in transcript. |

No command handler should look up a process by global `session_id` unless that
lookup is delegated through the owning `AgentThread.ToolResourceBag`.

### `browser_script` And Browser Commands

Keep:

- command/start/observe/cancel shape
- CDP/browser script backend logic where still useful
- artifact and image persistence

Change:

- `BrowserManager` owns sessions, not process-wide `session_id` statics
- `BrowserHandle` owns script runs
- browser actions serialize per browser, not per whole app
- `BrowserHandle.active_agent_id` enforces one running agent per browser
- SDK `Browser` object maps to `browser_id`
- TUI "Local Chrome" display reads live browser state
- runtime journals browser events and script output

Browser ownership details:

```text
BrowserHandle
  lease: Mutex<Option<AgentId>>
  action_mutex: Mutex<()>
  status: watch<BrowserStatus>
  capture_owner: Mutex<Option<CaptureOwner>>
  script_runs: HashMap<ScriptRunId, ScriptRunHandle>
```

Rules:

- one active running agent can claim a browser
- observe/cancel for browser scripts route through `ScriptRunHandle`
- cancel preserves partial script output and artifact links
- browser capture start/stop has a clear owner and cannot leak across agents
- completed browser script runs remain visible until cleaned by policy
- managed/cloud browser cleanup happens through `BrowserManager`
- local/keep-alive browser lifetime can outlive an agent but not the runtime

### Python Tool

Keep:

- `PythonWorker`
- persistent namespace behavior
- artifact/image output mapping

Change:

- worker lifetime is `AgentThread`, not one model run
- follow-ups reuse the same worker
- worker events stream through `RuntimeEventBus`
- cancellation and timeout are controlled by the agent turn token
- agent close shuts down the worker
- Python handler uses the worker's evented run API, not a blocking one-shot call
- timeout restarts the worker when required by worker protocol semantics
- `attempt.cancel` is wired to the active turn cancellation token
- stdout/stderr/image/artifact events are projected live and journaled

### MCP

Keep:

- stdio/http connection code
- tool handler shape

Change:

- connection manager lifetime belongs to runtime/profile/agent, not provider call
- SDK and TUI can see MCP connection status through runtime events
- shutdown is explicit and journaled
- partial startup is represented explicitly: some servers may be connected while
  others failed
- per-server errors are live events and journal records
- namespaced tool parsing stays inside the MCP manager
- read-only parallel gating remains enforced by tool metadata
- stdio child processes and read tasks shut down on agent/runtime close
- HTTP/SSE session ids and stream parsing remain MCP-manager responsibilities
- pending MCP calls define cancel/shutdown behavior before implementation starts

### Subagents

Replace store-backed subagent live logic with `AgentControl`.

Tool handlers should become thin:

```text
spawn_agent      -> ctx.runtime.agent_control(ctx.agent_id).spawn(...)
wait_agent       -> ctx.runtime.agent_control(ctx.agent_id).wait(...)
send_input       -> ctx.runtime.agent_control(ctx.agent_id).send(...)
followup_task    -> ctx.runtime.agent_control(ctx.agent_id).followup(...)
list_agents      -> ctx.runtime.agent_control(ctx.agent_id).list(...)
close_agent      -> ctx.runtime.agent_control(ctx.agent_id).close(...)
resume_agent     -> ctx.runtime.agent_control(ctx.agent_id).resume(...)
```

The store writes move inside runtime/journal adapters.

Subagent invariants:

- `AgentControl` is the only live authority for spawn/list/wait/send/close
- root tree path/nickname/role metadata is live memory plus `StateIndex`
- full-history children inherit agent type, model, reasoning effort, shell
  snapshot, sandbox/approval policy, and relevant environment metadata
- spawn validates capacity before materializing a child
- strict mode rejects over-capacity immediately with a typed error
- optional queue mode creates visible queued children and journals queue state
- child completion journals status, enqueues non-triggering mailbox mail, and
  keeps the child open until close policy runs
- parent wait never consumes child output as direct tool output
- close parent closes or cancels descendants according to explicit runtime policy
- resume root rehydrates open descendant edges and pending mailbox items

### TUI

Current:

```text
App owns Store
TUI writes session.input
TUI starts OS thread
OS thread runs run_session_with_config
TUI refreshes store cache from StoreNotification
transcript derives live state from DB events
```

Target:

```text
App owns RuntimeHandle + JournalReader
submit creates AgentThread through runtime
runtime emits live events
TUI updates active projection from RuntimeEventBus
SQLite hydrates history/sidebar/resume
```

TUI should still persist all history, but active rendering must not depend on a
DB refresh loop.

TUI paths that must be rewired:

- initial submit and run start
- follow-up input
- queued follow-ups
- mailbox continuation prompts
- goal continuation prompts
- stop/cancel
- rollback
- auth-nudge resume
- history overlay resume/open
- subagent panel and status projection
- developer/actions/browser overlays that currently read store-derived state
- `AppStateCache` refreshes that currently imply active status

The TUI can still hydrate old sessions from SQLite, but once a session is active
again, live status comes from runtime projections.

### CLI

CLI commands split into:

```text
live commands:
  run
  followup
  stop
  resume
  spawn-related smoke tests
  browser_script live execution
  Python live execution
  dataset/eval task execution that creates live agents

journal commands:
  history
  show
  export
  auth/settings
  artifacts
  profile/cookie sync when not tied to active browser handles
  import/export/trace inspection
  cleanup
```

Live commands use `BrowserUseRuntime`. Journal commands use `Store`.

Any CLI command that can affect a running process, browser, MCP connection,
subagent, mailbox item, or cancellation token is a live command. Any command
that only reads or mutates durable config/history/debug data can stay
store-backed.

### Python SDK

Add `sdk-server` backed by the same runtime:

```text
Python process
  -> one RuntimeClient
  -> stdio JSON-RPC
  -> Rust sdk-server
  -> BrowserUseRuntime
```

Required JSON-RPC methods:

```text
runtime.ping
browser.create
browser.start
browser.stop
browser.close
agent.create
agent.run
agent.stream
agent.followup
agent.stop
agent.close
history.read
```

This is only good if the server wraps the new live runtime. A stdio server over
the current CLI/store path would just expose the same bad coordination model.

## Direct Build Strategy

This is a one-branch, direct-to-target rewrite. Ordered checkpoint commits are
fine for review and bisectability, but the branch should not treat intermediate
coexistence as a product state.

Rules:

- The new runtime is the only live authority.
- Old public APIs may exist only as wrappers over the new runtime.
- Old store-first live paths are deleted or made unreachable in the same branch.
- TUI/CLI history can keep using existing Store readers.
- TUI/CLI/SDK live behavior must use `BrowserUseRuntime`.
- Existing event names can be mapped from typed runtime events.
- Unsupported SDK compatibility features fail loudly.
- The branch is not complete while SQLite, TUI child threads, and the runtime
  can all independently schedule live agent behavior.

## End-State Code Contract

The final codebase should look like this. This is not a rollout sequence; it is
the target shape to implement directly.

### Runtime API

Add a runtime crate or module:

```text
preferred:
  crates/browser-use-runtime

acceptable if crate split is too noisy:
  crates/browser-use-agent/src/runtime/
```

It exposes:

- `BrowserUseRuntime`
- `RuntimeHandle`
- `RuntimeEvent`
- `RuntimeEventBus`
- `RuntimeEventProjection`
- `LiveThreadPersistence`
- `StateIndex`
- `JournalSink`
- `JournalReader`
- `MemoryJournal`
- `SqliteJournal`
- `Durability`
- runtime id types

`JournalSink`, `JournalReader`, `MemoryJournal`, `SqliteJournal`, and
`Durability` are part of the runtime core from the start. Journaling is not a
late adapter because barrier writes define correctness for accepted user input,
mailbox delivery, spawn edges, browser claims, and terminal statuses.

### Agent Runtime State

Runtime-owned agent state:

- `AgentThread`
- `AgentStatus`
- `InMemoryHistory`
- `InputQueue`
- follow-up queue
- mailbox queue with delivery phases
- cancellation tree
- scoped `ToolResourceBag`

`StoreTurnState` is not a live default path. Its useful logic becomes replay
and compatibility helpers behind `JournalReader`/`LiveThreadPersistence`.

### Tool Resource Ownership

Every active agent has a `ToolResourceBag`:

- per-agent `UnifiedExecManager`
- per-agent `PythonWorker`
- per-agent or per-profile `McpConnectionManager`
- approval cache
- tool-search/cache state

`ToolCtx` includes agent/session/run ids, optional browser id, and
`RuntimeHandle`. Provider/tool registry construction receives resources from
`AgentThread`, not ad hoc session-id globals.

### Browser Runtime State

Runtime-owned browser state:

- `BrowserManager`
- `BrowserHandle`
- `BrowserScriptRunHandle`
- browser claim/release lease
- per-browser action serialization
- browser capture ownership

`browser-use-browser` no longer exposes process-wide session/script statics as
public live state. If temporary internal statics remain, only `BrowserManager`
can address them.

### AgentControl And Subagents

Runtime-owned subagent state:

- root-tree `AgentControl`
- path/name/role registry
- spawn validation
- fork history materialization
- strict Codex capacity scheduler
- optional explicit queued scheduler mode
- mailbox router
- mailbox delivery phases
- wait/send/followup/close/resume/list
- spawn-edge persistence through `StateIndex`
- descendant shutdown/resume

Store-backed wait/spawn/mailbox live logic is deleted or made unreachable.
Subagent tools become thin calls into `AgentControl`.

### Persistence And Replay

SQLite-backed persistence is split by role:

- `LiveThreadPersistence` for durable thread items and resume materialization
- `StateIndex` for metadata, spawn edges, archive/list/search/feedback
- `JournalSink` for complete event/debug history

Replay restores transcript, pending mail, agent tree, goals, config, durable
artifacts, and metadata. Replay does not restore stale live resource handles.
Lost resources are marked with explicit journal events.

### Runtime Event Projection

App-visible event delivery is:

- runtime snapshot API
- per-surface subscriptions
- serialized listener task per connection
- history snapshot plus live event ordering barrier
- final projected event before request resolution
- raw debug stream opt-in
- idle detach/unload policy

TUI, SDK, and future app-server surfaces consume projections by default, not raw
internal event broadcasts.

### TUI Integration

TUI owns:

- `RuntimeHandle`
- `JournalReader`
- live event subscription
- live transcript projection
- journal-backed history/sidebar hydration

These store-first live paths are gone:

- `run_agent_thread`
- `spawn_tui_child_agent`
- `ACTIVE_AGENT_RUNS`
- store-notification-driven active status

The TUI still reads old SQLite sessions for history, but active sessions render
from runtime projection.

### CLI Integration

Live CLI commands use `BrowserUseRuntime`:

- run
- followup
- stop
- resume/live continuation
- spawn-related smoke tests
- browser-script live execution
- Python live execution
- eval/dataset execution that creates live agents

Journal/config commands use `Store`/`StateIndex`:

- history
- show
- export
- auth/settings
- artifacts
- profile/cookie sync when not tied to active browser handles
- import/export/trace inspection
- cleanup

### SDK Integration

Add:

- `browser-use-terminal sdk-server --transport stdio`
- JSON-RPC router
- one stdout protocol writer
- stderr logs only
- event stream notifications
- Python `browser_use` compatibility package

The SDK server wraps `BrowserUseRuntime`. It must not scrape CLI text or launch
the old store-first run path.

### Required Deletions Or Demotions

The final branch deletes or makes unreachable:

- `StoreTurnState` default live path
- store-backed subagent wait/spawn/mailbox live code
- TUI child thread launcher
- DB status cancellation monitor
- provider-created global exec manager paths
- SDK CLI launcher path for agent runs
- direct Store polling as a wakeup/scheduling primitive

Existing APIs that need to survive call the runtime. They are wrappers, not
parallel implementations.

## Verification Plan

Required repo checks:

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
scripts/verify-terminal-ui.sh
```

Runtime tests:

```text
runtime creates root agent and journals session
runtime starts and completes fake agent
runtime cancellation cancels active turn
journal replay hydrates completed session
journal replay hydrates pending mailbox
journal replay marks stale live resources lost/orphaned
event bus delivers ordered live events
runtime projection serializes snapshot before live events
final projected event is delivered before request resolves
critical journal append failure surfaces as run failure/degraded status
```

Subagent tests:

```text
spawn_agent creates child AgentThread
strict capacity counts open spawned children and rejects immediately
optional queue mode queues visibly based on config
queued child starts only under explicit queue policy
child completion enqueues parent mailbox item
child completion does not trigger parent turn by default
wait_agent returns immediately when mail is pending
wait_agent wakes on mailbox seq change
wait_agent times out without deleting or hiding live child status
parent idle wake schedules continuation when trigger_turn=true
close_agent cancels descendants
resume_agent materializes durable child metadata without stale live handles
full-history fork rejects agent type/model/reasoning overrides
full-history fork filters parent rollout items correctly
```

Tool resource tests:

```text
exec_command process survives across turns in same agent
write_stdin cannot address another agent's process by default
agent close kills remaining exec processes
foreground exec_command is cancelled by turn cancel
background exec_command survives turn cancel and remains pollable
exec_command process is marked lost after crash/resume
terminal.interaction events preserve input/output ordering
python worker persists across follow-up
python worker closes on agent close
python worker namespace is recreated or marked lost after crash/resume
python streaming emits output deltas and artifacts
python timeout restarts worker when required
browser_script runs are scoped to BrowserHandle
browser_script cancel preserves partial output and artifacts
browser_script observe/cancel race is deterministic
browser lease/script run is recreated or marked orphaned after crash/resume
same browser concurrent agents fail fast
different browsers run in parallel
MCP manager shuts down on agent/runtime close
MCP partial startup reports per-server status
pending MCP calls have deterministic cancel/shutdown behavior
MCP transports are recreated or marked lost after crash/resume
```

TUI tests:

```text
subagents stay visible while running
queued subagents stay visible
wait timeout does not blank live subagent panel
child completion appears as status/mail without hiding the child
explicit follow-up/mail with trigger_turn wakes parent and displays continuation
pending/queued followups render correctly
goal continuation renders correctly
rollback updates live projection and history
auth-nudge resume routes through runtime
history overlay still reads old SQLite sessions
terminal smoke has no stale redraws or leaked escape sequences
```

Live model smoke tests:

```text
spin up 2 subagents ask capitals and compare answers
spin up 3 subagents research this codebase
spawn many subagents above concurrency limit
ask parent what happened after children complete
cancel parent with children running
run browser_script while subagents run
run exec_command background process then poll with write_stdin
```

SDK tests:

```text
Agent(task, llm).run()
Agent.add_new_task() preserves memory
two agents with different browsers run concurrently
two agents with same browser fail fast
agent.stop cancels Rust run
asyncio cancellation cancels Rust run
history.final_result()
structured output validation
```

CLI tests:

```text
run/followup/stop use runtime APIs
history/show/export read SQLite only
browser_script live command routes through BrowserManager
Python live command routes through AgentThread worker
eval/dataset execution does not create store-driven live sessions
cleanup does not kill unrelated live runtime resources
```

Event/replay tests:

```text
tool.output_delta/tool.output/tool.failed remain replayable
artifact.created appears before references
browser capture artifacts replay in transcript
mailbox delivered/consumed seq prevents duplicate resume delivery
StateIndex open/closed spawn edges list descendants correctly
SQLite postmortem can explain cancelled/failed/timed-out runs
```

## Expected Rewrite Size

This touches the core runtime but not every product subsystem.

High blast radius:

```text
crates/browser-use-agent/src/entrypoint
crates/browser-use-agent/src/entrypoint/provider.rs
crates/browser-use-agent/src/config_overrides.rs
crates/browser-use-agent/src/events/store_sink.rs
crates/browser-use-agent/src/infra/persistence.rs
crates/browser-use-agent/src/session
crates/browser-use-agent/src/session/reconstruct.rs
crates/browser-use-agent/src/session/resume.rs
crates/browser-use-agent/src/session/rollback.rs
crates/browser-use-agent/src/compact
crates/browser-use-agent/src/subagents
crates/browser-use-agent/src/tools/handlers/subagent.rs
crates/browser-use-agent/src/tools/registry.rs
crates/browser-use-agent/src/tools/runtime.rs
crates/browser-use-tui/src/runtime.rs
crates/browser-use-tui/src/main.rs
crates/browser-use-tui/src/transcript.rs
crates/browser-use-cli/src/main.rs
crates/browser-use-store/src/lib.rs
crates/browser-use-protocol
```

Medium blast radius:

```text
crates/browser-use-browser/src/lib.rs
crates/browser-use-agent/src/tools/handlers/browser.rs
crates/browser-use-agent/src/tools/handlers/python.rs
crates/browser-use-agent/src/tools/handlers/shell.rs
crates/browser-use-agent/src/tools/unified_exec.rs
crates/browser-use-agent/src/mcp
crates/browser-use-agent/src/goals
crates/browser-use-python-worker/src/lib.rs
python package layout
```

Low blast radius:

```text
model provider clients
LLM schema types
pure config parsing
prompt text except subagent guidance
browser-use-python-worker protocol internals
tool implementations that are stateless
```

Estimated size:

```text
Strong one-shot rewrite: 2-4 weeks
Working but rough one-shot: 1-2 weeks
Full SDK parity on top: additional 2-4 weeks
```

The rewrite is large because it changes ownership, not because every tool must
be reimplemented.

## Non-Negotiables

- One live runtime authority.
- No SQLite live mailbox.
- No DB-driven agent wakeups.
- No TUI child OS thread launcher reopening the same state dir.
- No CLI scraping for SDK.
- Every live transition journaled for postmortem debugging.
- Runtime event stream and SQLite journal must agree.
- Tests must prove explicit parent wakeups, wait semantics, browser ownership,
  event ordering, and tool resource cleanup.

## Final Architecture Summary

```text
             Browser Use Terminal / CLI / Python SDK / future app-server
                                      |
                                      v
                             BrowserUseRuntime
                                      |
        +-----------------------------+------------------------------+
        |                             |                              |
        v                             v                              v
   AgentManager                 BrowserManager              RuntimeEventBus
        |                             |                              |
        v                             v                              v
   AgentThread                  BrowserHandle          RuntimeEventProjection
        |                             |                              |
        +-- AgentControl             |                              v
        +-- AgentMailbox             |                         TUI/SDK/app
        +-- TurnDriver               |
        +-- ToolResourceBag          |
        |     +-- UnifiedExecManager |
        |     +-- PythonWorker       |
        |     +-- McpConnectionManager
        |
        v
  +----------------------+----------------------+----------------------+
  | LiveThreadPersistence|      StateIndex      |     JournalSink      |
  +----------------------+----------------------+----------------------+
                                      |
                                      v
                        SQLite persistence and debug journal
```

This gives Codex-like live reliability, Browser Use-specific browser/SDK
capabilities, and keeps the external SQLite debug story intact.
