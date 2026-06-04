# Hybrid Mailbox Runtime Plan

Regression prompt:

```text
can you spin up a few subagents that research this codebase pls
```

Expected behavior:

- no red `spawn_agent failed` rows for normal role-based spawn attempts
- subagents start with distinct bounded tasks
- the parent does not answer with "I'll wait when you want" for a request that expects findings
- `wait_agent` waits on mailbox notifications, not UI lifecycle rows
- if the parent already answered, child completion mail still wakes the selected parent session through a mailbox continuation
- the TUI keeps enough subagent state visible to understand running, timed out, completed, and results-ready states

```text
Browser Use Terminal
        |
        v
AgentRuntime
        |
        +-- AgentControl facade
        |       spawn_agent()
        |       send_inter_agent_communication()
        |       wait_agent()
        |       list_agents()
        |       close_agent()
        |       resume_agent()
        |
        +-- Scheduler
        |       per-parent running limit
        |       global running limit
        |       queued -> running -> done/failed/cancelled
        |
        +-- SessionRuntime: /root
        |       active turn state
        |       live mailbox queue
        |       mailbox watch seq
        |       status watch
        |
        +-- SessionRuntime: /root/research_a
        |       live mailbox queue
        |       mailbox watch seq
        |       status watch
        |
        +-- SessionRuntime: /root/research_b
                live mailbox queue
                mailbox watch seq
                status watch

SQLite Store
        |
        +-- sessions
        +-- agent_edges
        +-- agent_messages / mailbox rows
        +-- events
        +-- artifacts

child finishes
        |
        +-- append parent agent.completed / agent.failed event
        +-- enqueue parent <subagent_notification> mailbox message
        +-- persist mailbox row
        +-- bump parent mailbox watch seq
        +-- active wait_agent wakes
        +-- idle selected parent gets mailbox-continuation wakeup
        +-- unselected/offline parent keeps pending mail for next turn/resume
```

## Goal

Implement Codex-style mailbox semantics while keeping Browser Use Terminal's SQLite lifecycle.

The target is not a full Codex thread lifecycle port. The target is a hybrid:

```text
mailbox = coordination
events = UI/history
SQLite = durability
runtime = wakeups/scheduling
```

The bug this fixes: a child can append `agent.completed` to the parent session while the parent model never sees the subagent result. `agent.completed` is useful for UI history, but the synchronization path must be mailbox mail plus a runtime wakeup.

## Codex Versus Browser Use Hybrid

```text
Codex-style shape

model tool call
    |
    v
AgentControl
    |
    +-- ThreadManager
    |       thread lifecycle
    |       active/background state
    |       status subscriptions
    |
    +-- Mailbox
            in-memory wakeups
            notification delivery
            wait_agent subscription


Browser Use hybrid target

model tool call
    |
    v
AgentControl
    |
    +-- Runtime Mailbox
    |       fast wakeups while loaded
    |       wait_agent subscription
    |
    +-- SQLite Mailbox Ledger
    |       agent_messages pending/consumed rows
    |       crash/restart durability
    |
    +-- Scheduler
    |       queued/running limits
    |       child start promotion
    |
    +-- TUI Projection
            lifecycle rows from events
            results-ready mailbox state
            parent continuation wakeups
```

Browser Use should keep SQLite as the source of truth. Codex's in-memory mailbox is better for live wakeups; Browser Use's SQLite mailbox is better for debugging, replay, and restart recovery. The target is both: live wakeups backed by durable rows.

## Non-Negotiable Semantics

1. `agent.completed` is not synchronization.

   It is a UI/history event only. The model synchronization path is mailbox delivery.

2. Completion notification is mailbox mail.

   When a child finishes, it sends the parent:

   ```text
   InterAgentCommunication {
       author: /root/child,
       recipient: /root,
       content: <subagent_notification>...</subagent_notification>,
       trigger_turn: false,
   }
   ```

3. `wait_agent` waits on mailbox activity.

   It returns:

   ```json
   { "message": "Wait completed.", "timed_out": false }
   ```

   when parent mailbox mail is already queued or arrives before the deadline. It does not return child content directly.

4. Prompt assembly drains mailbox content.

   The next parent prompt sees drained mailbox messages as contextual user messages and persists `agent.mailbox_input` events.

5. `trigger_turn` means "start an idle target for a delegated task."

   - Initial child task: `trigger_turn = true`
   - `followup_task`: `trigger_turn = true`
   - `send_message`: `trigger_turn = false`
   - child completion notification: `trigger_turn = false`

   Completion notifications are the special non-trigger mail that may still wake
   the parent through the mailbox-continuation path. They do not become ordinary
   user follow-up text; prompt assembly drains them as mailbox context.

6. Late child completion must not be lost.

   If the parent is idle or has already written `session.done`, completion mail still remains queued. Only closed/deleted parent state should suppress delivery.

## Phase 0: Parity Tests First

Add failing tests for the exact behavior we need before changing internals.

Agent/tool tests:

- child completion queues parent `<subagent_notification>` mailbox mail
- `wait_agent` returns immediately when parent mailbox already has mail
- `wait_agent` wakes when completion mail arrives during the wait
- `wait_agent` still does not return child content directly
- late completion after parent `session.done` is still queued
- interrupted child does not notify parent
- follow-up child completion notifies parent every turn

Session reconstruction tests:

- drained mailbox mail becomes `agent.mailbox_input`
- `agent.mailbox_input` reconstructs into provider context
- subagent notification context is not duplicated as ordinary assistant text

TUI tests:

- parent transcript shows compact started/running/done lifecycle state
- timeout does not make running subagents disappear
- completed subagents remain visible as "results ready" or done state

## Phase 1: Durable Mailbox Correctness

Make `agent_messages` the durable mailbox, not optional decoration.

Changes:

- remove the active-parent guard from completion mail delivery
- keep closed-child/closed-parent safeguards
- ensure every child terminal status sends parent mailbox mail exactly once per child run
- persist completion mail with `trigger_turn = false`
- keep appending `agent.completed` / `agent.failed` events for UI/history

Acceptance:

- after a child finishes, `agent_messages` contains a parent-targeted `<subagent_notification>`
- parent `agent.completed` event and mailbox message agree on child id, path, and status
- duplicate completion callbacks do not enqueue duplicate mail

## Phase 2: Correct `wait_agent`

Make `wait_agent` mailbox-driven.

Behavior:

```text
wait_agent(parent):
    if parent mailbox has pending rows:
        return completed

    subscribe to mailbox/store notification seq
    until deadline:
        if parent mailbox has pending rows:
            return completed
        wait for seq change

    return timed_out
```

Important:

- do not drain mailbox inside `wait_agent`
- do not use `agent.completed` as the wait condition
- do not return child result content directly
- keep v2 targetless `wait_agent` semantics

Acceptance:

- if children finished before the user asks "what happened?", the next `wait_agent` returns completed immediately
- if a child finishes while `wait_agent` is pending, it wakes immediately
- if no mail arrives, it times out normally

## Phase 3: Turn-Boundary Mailbox Delivery

Make prompt assembly drain mailbox content with Codex-like turn-boundary behavior.

Rules:

- before answer boundary: current-turn mailbox delivery is allowed
- after answer boundary: mailbox delivery is deferred to the next turn
- `trigger_turn = true` can start an idle target session
- `trigger_turn = false` wakes `wait_agent`
- child completion mail can schedule a parent mailbox continuation when the parent is idle and selected in the TUI
- ordinary non-trigger peer mail remains pending until a turn is otherwise started

Implementation notes:

- keep or refine existing `MailboxDeliveryPhase`
- append `agent.mailbox_input` when draining durable mailbox rows
- mark rows consumed instead of deleting them, so SQLite remains useful for replay/debugging
- keep `agent.mailbox_input` events as the prompt-visible drain record

Acceptance:

- completion after parent answer boundary stays pending
- a later parent turn sees the `<subagent_notification>`
- follow-up user input reopens deferred mailbox delivery
- trigger-turn mail starts child work without manual user input

## Phase 4: Live Runtime Mailbox

Introduce first-class runtime mailboxes while keeping SQLite as the ledger.

New structures:

```text
AgentRuntime
    store
    sessions: HashMap<SessionId, SessionRuntime>
    scheduler

SessionRuntime
    session_id
    mailbox
    status_tx
    active_turn_state

Mailbox
    pending_message_ids
    seq_tx
    send()
    has_pending()
    has_pending_trigger_turn()
    drain_for_prompt()
    subscribe()
```

Mailbox send path:

```text
send(message):
    insert durable agent_messages row
    push row id into live queue if session runtime exists
    bump live seq
    notify store watchers
```

Hydration:

- on app startup or session resume, load unconsumed mailbox rows
- populate the runtime mailbox pending queue
- bump or initialize seq so `wait_agent` can see pending rows immediately

Acceptance:

- live `wait_agent` wakes without DB polling
- after process restart, pending mailbox rows are still visible
- no mail is lost if a child completes while the parent runtime is not loaded

## Phase 5: AgentControl Facade

Move subagent tools onto a Codex-shaped facade.

API:

```text
AgentControl::spawn_agent_with_metadata(...)
AgentControl::send_inter_agent_communication(...)
AgentControl::subscribe_status(...)
AgentControl::get_status(...)
AgentControl::resolve_agent_reference(...)
AgentControl::shutdown_live_agent(...)
AgentControl::resume_agent(...)
```

Tool mapping:

- `spawn_agent` creates child session + edge, then sends initial task to child mailbox with `trigger_turn = true`
- `send_message` sends mailbox mail with `trigger_turn = false`
- `followup_task` sends mailbox mail with `trigger_turn = true`
- `wait_agent` waits on parent mailbox seq
- `list_agents` reads runtime status plus durable store fallback
- `close_agent` updates runtime and durable edge state

Acceptance:

- tools no longer mix direct store event heuristics with mailbox semantics
- runtime and durable state transitions are centralized
- `SubagentManager` becomes either a thin facade or is replaced by `AgentRuntime`

## Phase 6: Scheduler And Backpressure

Support many subagents without running all of them at once.

Scheduler state:

```text
queued
starting
running
done
failed
cancelled
closed
```

Limits:

- `max_running_per_parent`, default 4
- `max_running_global`, default 16 or 32
- optional per-role limits later

Behavior:

- spawn creates queued child if limits are saturated
- scheduler starts queued children when slots open
- `list_agents` exposes queued/running/done state
- TUI shows compact aggregate state, e.g. `4 running, 12 queued, 8 done`

Acceptance:

- spawning 50 subagents does not start 50 model calls
- waiters wake as individual children complete
- queued children survive restart

## Phase 7: TUI State And Observability

Make the TUI show mailbox/runtime state clearly.

Display goals:

- compact lifecycle grouping instead of one blank-spaced row per lifecycle event
- persistent subagent summary while children are live or queued
- show completed subagent count/results-ready state after timeout
- make timeout visually mean "still waiting can continue", not "dead end"

Examples:

```text
subagents
  running  Harvey, Ramanujan
  done     Ohm
  queued   5

subagents
  done     Harvey, Ohm, Ramanujan
  mail     3 results ready
```

Acceptance:

- the user can tell whether subagents are running, queued, done, or waiting to be consumed
- timeout does not make the UI look empty
- finished rows do not appear without any visible next state

Current implementation checkpoint:

- lifecycle rows remain visible
- timed-out waits remain visible
- pending completion mail creates a `subagent results ready` row
- selected done parent sessions resume through `session.mailbox_continuation.started`

## Phase 8: Live Model Verification

Run real end-to-end scenarios.

Scenarios:

1. Spawn 3 research subagents and wait.

   Expected:

   - parent calls `wait_agent`
   - wait wakes when first completion mail arrives
   - model eventually summarizes all requested results

2. Spawn 3 research subagents with a too-short wait.

   Expected:

   - timeout is visible
   - completions enqueue mailbox mail
   - follow-up `wait_agent` returns completed immediately
   - next prompt sees notifications

3. Spawn many subagents.

   Expected:

   - only limit number run concurrently
   - rest are queued
   - scheduler starts next when slots free
   - UI stays understandable

4. Restart app with pending subagent mail.

   Expected:

   - pending mailbox rows hydrate
   - `wait_agent` returns completed immediately
   - prompt sees notifications

## Implementation Checklist

Done on this branch:

- completion handler enqueues durable parent mailbox mail even after parent `session.done`
- CLI child completion mirror uses the same durable-mail rule
- v2 `wait_agent` waits on pending mailbox rows
- prompt assembly drains mailbox rows into `agent.mailbox_input`
- consumed mailbox rows are retained with `consumed_ms`
- `agent_type`/model/reasoning metadata with default/full fork normalizes into a non-forked role spawn instead of failing
- default v2 wait timeout is long enough for research-style subagents
- TUI can resume a selected done parent when pending mailbox mail arrives

Still required for the full target:

- move scattered subagent store/runtime operations behind `AgentControl`
- replace manager-only live mailbox with a hydrated runtime mailbox backed by `agent_messages`
- add scheduler/backpressure so excess subagents queue instead of rejecting or starting unlimited work
- add compact aggregate TUI grouping for many agents, not just lifecycle rows
- add restart hydration tests for pending mailbox rows
- add many-subagent live smoke tests once scheduler exists

## Things To Avoid

- Do not make `agent.completed` the `wait_agent` condition.
- Do not turn ordinary non-trigger peer mail into user follow-up text.
- Do not lose mailbox rows because parent session is `done`.
- Do not return child result content directly from v2 `wait_agent`.
- Do not start unlimited subagent model calls.
- Do not bury mailbox state inside TUI-only events.

## First Milestone

The first useful milestone is not the full runtime. It is:

```text
durable completion mail
    + mailbox-driven wait_agent
    + prompt drain of queued notifications
    + tests for late completion after parent session.done
```

That milestone fixes the observed failure and establishes the real mailbox contract. After that, introduce `AgentRuntime`, then scheduler/backpressure.
