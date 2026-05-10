# Codex Tool Compatibility Audit

Status: Phase 0 implementation contract for `docs/codex-parity-agent-tui-plan.md`.

This audit answers the first required implementation task:

```text
Audit Codex and current Python tools, then write the target tool compatibility table.
```

The target is Codex-like raw agent ability with simpler internals:

- keep the Codex-facing names and behavior where they matter
- port the useful Python main-branch coding tools into Rust
- keep the browser tool Python-owned
- do not copy Codex sandboxing, approval policy, guardian process, or app-server protocol

## Sources Reviewed

Codex:

- `/Users/greg/Downloads/tmp/codex/codex-rs/tools/src/local_tool.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/tools/src/apply_patch_tool.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/tools/src/view_image.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/tools/src/plan_tool.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/tools/src/agent_tool.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/tools/src/tool_registry_plan.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/tools/registry.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/tools/events.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/tools/handlers/unified_exec.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/core/src/unified_exec/mod.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/apply-patch/src/lib.rs`
- `/Users/greg/Downloads/tmp/codex/codex-rs/file-search/src/lib.rs`

Current Python main branch:

- `/Users/greg/Documents/browser-use/experiments/llm-browser/src/llm_browser/tool/builtins.py`
- `/Users/greg/Documents/browser-use/experiments/llm-browser/src/llm_browser/tool/registry.py`
- `/Users/greg/Documents/browser-use/experiments/llm-browser/src/llm_browser/tool/shell.py`
- `/Users/greg/Documents/browser-use/experiments/llm-browser/src/llm_browser/tool/files.py`
- `/Users/greg/Documents/browser-use/experiments/llm-browser/src/llm_browser/tool/python_browser.py`
- `/Users/greg/Documents/browser-use/experiments/llm-browser/src/llm_browser/tool/browser_exports.py`
- `/Users/greg/Documents/browser-use/experiments/llm-browser/src/llm_browser/tool/session.py`
- `/Users/greg/Documents/browser-use/experiments/llm-browser/src/llm_browser/agent/service.py`
- `/Users/greg/Documents/browser-use/experiments/llm-browser/tests/test_shell_file_tools.py`

Current Rust rewrite:

- `crates/browser-use-core/src/lib.rs`
- `crates/browser-use-protocol/src/lib.rs`
- `crates/browser-use-store/src/lib.rs`
- `crates/browser-use-python-worker/src/lib.rs`
- `crates/browser-use-tui/src/main.rs`

## Current State Summary

The Rust rewrite already has:

- SQLite-backed sessions/events/artifacts
- model provider loop
- Python browser tool named `python`
- `done`
- subagent-ish tools:
  - `spawn_agent`
  - `wait_agent`
  - `send_message`
  - `followup_task`
  - `list_agents`
  - `close_agent`
- TUI screens for setup/workbench/running/result/failure/browser/history/actions

The Rust rewrite does not yet have the Codex-grade coding tool surface:

- no Rust `exec_command`
- no Rust `write_stdin`
- no Rust `apply_patch`
- no Rust `read_file`
- no Rust `search_files`
- no Rust `list_files` / fuzzy file search tool
- no Rust `view_image`
- no `update_plan`
- no central `ToolRegistry` abstraction separate from `browser_tool_specs()` and `dispatch_tool_call()`

The Python main branch already has useful coding tools that should be ported rather than discarded:

- `exec_command` alias
- `write_stdin` alias
- synchronous `shell`
- long-running `shell_start` / `shell_poll` / `shell_stdin` / `shell_stop`
- `read`
- `write`
- `edit`
- `apply_patch`
- `glob`
- `grep`
- Codex-shaped `spawn_agent` / `wait_agent` / `close_agent`

## Target Tool Table

| Target tool | Codex equivalent | Python main equivalent | Rust current | Backend owner | Decision |
| --- | --- | --- | --- | --- | --- |
| `exec_command` | yes | yes, alias to shell/start | missing | Rust | implement first |
| `write_stdin` | yes | yes, alias to managed process stdin/poll | missing | Rust | implement first |
| `apply_patch` | yes, freeform grammar | yes, JSON patch string with Codex patch support | missing | Rust | implement first; prefer freeform |
| `read_file` | internal/read support exists, plus shell usage | `read` | missing | Rust | implement as Codex-like file read |
| `search_files` | rg-backed grep/search helpers | `grep` | missing | Rust | implement with `rg` |
| `list_files` | fuzzy file search crate and file listing | `glob` / directory `read` | missing | Rust | implement simple first, fuzzy later |
| `view_image` | yes | browser images indirectly through tool result | missing | Rust | implement after file tools |
| `python` | no direct Codex equivalent | yes, browser harness tool | exists | Python | keep and harden |
| `done` | final answer handled by agent protocol | yes | exists | Rust | keep |
| `update_plan` | yes | no dedicated equivalent | missing | Rust | add simple event-backed version |
| `spawn_agent` | yes | yes | exists with different shape | Rust | align closer to Codex v1 |
| `send_input` | yes | no direct target name | missing | Rust | add Codex v1 name |
| `wait_agent` | yes | yes | exists | Rust | keep, align response shape |
| `close_agent` | yes | yes | exists | Rust | keep, align response shape |
| `send_message` | Codex v2 | partial current Rust | exists | Rust | keep internal/optional, not primary |
| `followup_task` | Codex v2 | partial current Rust | exists | Rust | keep internal/optional, not primary |
| `list_agents` | Codex v2 | no primary v1 equivalent | exists | Rust | keep optional |
| `shell` | legacy Codex shell exists but unified exec is preferred | yes | missing | Rust | do not expose by default once `exec_command` exists |
| `shell_start` | no, replaced by `exec_command` returning session id | yes | missing | Rust | do not expose by default |
| `shell_poll` | no, replaced by empty `write_stdin` | yes | missing | Rust | do not expose by default |
| `shell_stdin` | no, replaced by `write_stdin` | yes | missing | Rust | do not expose by default |
| `shell_stop` | no exact Codex model tool | yes | missing | Rust | implement internal cancel/stop, not primary model tool |
| `write` | no preferred Codex equivalent | yes | missing | Rust | do not expose by default; use `apply_patch` |
| `edit` | no preferred Codex equivalent | yes | missing | Rust | do not expose by default; use `apply_patch` |
| `glob` | fuzzy/file search | yes | missing | Rust | replace with `list_files` |
| `grep` | rg-backed search | yes | missing | Rust | replace with `search_files` |
| `session` | no, Codex uses subagent tools | yes | missing | Rust | do not expose by default |

## Final Model-Visible Tool Set

Expose these by default:

```text
exec_command
write_stdin
apply_patch
read_file
search_files
list_files
view_image
python
done
update_plan
spawn_agent
send_input
wait_agent
close_agent
```

Keep these internal or compatibility-only:

```text
shell
shell_start
shell_poll
shell_stdin
shell_stop
write
edit
glob
grep
session
send_message
followup_task
list_agents
resume_agent
```

Rationale:

- Codex's raw ability comes from the first list.
- The second list creates a larger concept surface and should not be in the default prompt unless an old provider path requires it.

## Tool Contracts

### `exec_command`

Target schema:

```text
cmd: string, required
workdir: string, optional
shell: string, optional
tty: boolean, optional
login: boolean, optional
yield_time_ms: integer, optional
max_output_tokens: integer, optional
```

Deliberately ignored or rejected:

```text
sandbox_permissions
justification
prefix_rule
environment_id
```

Behavior:

- run from the task cwd by default
- resolve relative `workdir` against the task cwd
- stream command output into typed events
- wait up to `yield_time_ms` when provided
- return completed output if the command exits
- return a numeric or string `session_id` if the process is still running
- support `tty=true` for interactive commands
- cap returned output by `max_output_tokens`

Target output shape:

```json
{
  "session_id": "proc_...",
  "running": true,
  "output": "...",
  "metadata": {
    "exit_code": null,
    "duration_ms": 250,
    "truncated": false
  }
}
```

For completed commands:

```json
{
  "session_id": null,
  "running": false,
  "output": "...",
  "metadata": {
    "exit_code": 0,
    "duration_ms": 123,
    "truncated": false
  }
}
```

Events:

```text
tool.started
command.started
command.output
command.waiting
command.finished
command.failed
tool.finished
tool.failed
```

Implementation notes:

- Port the Python main-branch process behavior, but make the model interface Codex-shaped.
- The first implementation can use threads and `std::process`; a later implementation can use Tokio.
- PTY support should be added before this is considered done because Codex's feel depends on interactive command sessions.

### `write_stdin`

Target schema:

```text
session_id: string, required
chars: string, optional
yield_time_ms: integer, optional
max_output_tokens: integer, optional
```

Behavior:

- empty `chars` means poll
- non-empty `chars` writes to stdin, then polls
- return recent output and running status
- close/cleanup the process once it exits

Events:

```text
tool.started
command.output
command.finished
command.failed
tool.finished
tool.failed
```

Implementation notes:

- This replaces Python `shell_poll` and `shell_stdin` in the default model surface.
- Internally we still need process stop/cancel support.

### `apply_patch`

Target schema:

Prefer Codex freeform:

```text
*** Begin Patch
*** Add File: path
+content
*** Update File: path
@@
-old
+new
*** Delete File: path
*** End Patch
```

Compatibility schema if the provider cannot use freeform tools:

```text
input: string
```

or temporarily:

```text
patch: string
check: boolean
```

Behavior:

- support Codex Add File
- support Codex Delete File
- support Codex Update File
- support Codex Move to
- reject absolute paths
- reject paths outside cwd
- emit file-change events
- return changed paths and errors clearly

Events:

```text
tool.started
patch.started
patch.file_changed
patch.finished
patch.failed
tool.finished
tool.failed
```

Implementation notes:

- Best option: vendor or closely port `codex-rs/apply-patch`.
- The Python main parser is useful but less complete than Codex's parser.
- `write` and `edit` should not be exposed by default once this works.

### `read_file`

Target schema:

```text
path: string, required
start_line: integer, optional
end_line: integer, optional
max_bytes: integer, optional
max_lines: integer, optional
```

Compatibility accepted initially:

```text
line_offset
line_limit
offset
limit
```

Behavior:

- read UTF-8 with replacement for invalid sequences
- reject binary with a clear response
- line range output should include line numbers
- directory reads should return compact entries only if `list_files` is not yet implemented
- make truncation explicit

Events:

```text
tool.started
file.read
tool.finished
tool.failed
```

Implementation notes:

- Port Python main-branch line windows, binary detection, and missing-path suggestions.
- Prefer a model-facing name of `read_file` over Python's `read`.

### `search_files`

Target schema:

```text
query: string, required
path: string, optional
glob: string, optional
context_lines: integer, optional
max_results: integer, optional
```

Behavior:

- use `rg` first
- respect ignore rules by default
- skip noisy directories like `.git`, `target`, `.venv`, `node_modules`
- return file path, line number, and compact matching line
- make truncation explicit

Events:

```text
tool.started
file.search
tool.finished
tool.failed
```

Implementation notes:

- This replaces Python `grep` in the default model surface.
- Keep literal search first; regex support can be added later if needed.

### `list_files`

Target schema:

```text
path: string, optional
pattern: string, optional
max_results: integer, optional
recursive: boolean, optional
```

Behavior:

- list files/directories compactly
- support glob-like filtering initially
- respect ignore rules
- later add fuzzy search using Codex's `ignore` + `nucleo` pattern

Events:

```text
tool.started
file.list
tool.finished
tool.failed
```

Implementation notes:

- This replaces Python `glob` and directory `read` as the main discovery tool.

### `view_image`

Target schema:

```text
path: string, required
detail: string, optional, only "original"
```

Behavior:

- read local image
- return model-visible image data where supported
- store/render artifact reference for the TUI
- preserve `detail=original` when requested

Events:

```text
tool.started
artifact.image
tool.finished
tool.failed
```

Implementation notes:

- This is separate from browser screenshots, but browser screenshots can share the same artifact rendering path.

### `python`

Target schema:

```text
code: string, required
headless: boolean, optional
```

Behavior:

- stay Python-owned
- persistent namespace per task
- raw CDP helpers
- harnesless-compatible helper names
- reconnect awareness
- target/tab invalidation surfaced to the model
- screenshot attachment to next model continuation
- artifact write/copy/upload helpers

Events:

```text
tool.started
browser.connected
browser.reconnected
browser.disconnected
browser.target_changed
browser.live_url
browser.action
browser.screenshot
browser.artifact
tool.finished
tool.failed
```

Implementation notes:

- The current Rust rewrite has a Python worker island already.
- The next work is not to replace Python; it is to make its events and model output contract stronger.

### `done`

Target schema:

```text
result: string, optional
path: string, optional
```

Behavior:

- finish the current task
- if `path` is provided, load final answer content when safe
- emit final result event

Events:

```text
tool.started
tool.finished
task.finished or session.done
```

Implementation notes:

- Keep current Rust `done`, but consider `path` compatibility from Python main branch.

### `update_plan`

Target schema:

```text
explanation: string, optional
plan: array of { step: string, status: "pending" | "in_progress" | "completed" }, required
```

Behavior:

- validate at most one `in_progress`
- emit `plan.updated`
- return empty/small tool output
- TUI can render compact progress in running view

Events:

```text
tool.started
plan.updated
tool.finished
```

Implementation notes:

- Copy Codex's simple schema and avoid goal/budget machinery.

### Subagent tools

Default target surface should be Codex v1:

```text
spawn_agent
send_input
wait_agent
close_agent
```

`spawn_agent` target schema:

```text
agent_type: string, optional, "default" | "explorer" | "worker"
message: string, optional
items: array, optional
fork_context: boolean, optional
model: string, optional
reasoning_effort: string, optional
```

`send_input` target schema:

```text
target: string, required
message: string, optional
items: array, optional
interrupt: boolean, optional
```

`wait_agent` target schema:

```text
targets: array of string, required
timeout_ms: integer, optional
```

`close_agent` target schema:

```text
target: string, required
```

Events:

```text
tool.started
agent.spawned
agent.message
agent.finished
agent.failed
agent.cancelled
tool.finished
tool.failed
```

Implementation notes:

- Rust rewrite already has a v2-ish path model with `send_message`, `followup_task`, and `list_agents`.
- For Codex parity, add `send_input` and align `spawn_agent` / `wait_agent` / `close_agent` response shapes.
- Keep `send_message`, `followup_task`, and `list_agents` as optional compatibility tools, but do not make them central in the initial model instructions.

## Event Compatibility Table

| Capability | Current Rust event | Target event additions |
| --- | --- | --- |
| Python tool starts/finishes | `tool.started`, `tool.finished`, `tool.failed` | keep |
| Python worker output | existing worker-specific events | normalize key browser/action events |
| Command execution | missing | `command.started`, `command.output`, `command.waiting`, `command.finished`, `command.failed` |
| Patch editing | missing | `patch.started`, `patch.file_changed`, `patch.finished`, `patch.failed` |
| File read/search/list | missing | `file.read`, `file.search`, `file.list` |
| Plan updates | missing | `plan.updated` |
| Browser live URL | `browser.live_url` | keep |
| Browser open/reconnect request | `browser.open_requested`, `browser.reconnect_requested` | keep, add result/error events |
| Subagent spawn/update | `agent.spawned`, `agent.completed`, `agent.failed`, `agent.updated` | keep, add `agent.message` and Codex-shaped outputs |
| Final result | `session.done` | keep for now; later alias product name to `task.finished` in UI projection |

## Implementation Order From This Audit

### Slice 1: Registry extraction

Create a real Rust tool registry instead of hardcoding `browser_tool_specs()` and `dispatch_tool_call()`.

Minimum modules:

```text
crates/browser-use-core/src/tools/mod.rs
crates/browser-use-core/src/tools/registry.rs
crates/browser-use-core/src/tools/context.rs
crates/browser-use-core/src/tools/specs.rs
```

Exit:

- current `python`, `done`, and subagent tools still work through the registry
- no behavior regression

### Slice 2: Command tools

Implement:

```text
exec_command
write_stdin
```

Exit:

- long-running command can return a `session_id`
- `write_stdin` can poll and write
- output events render in the TUI at least as raw transcript blocks

### Slice 3: File/edit tools

Implement:

```text
apply_patch
read_file
search_files
list_files
view_image
```

Exit:

- model can inspect/edit the repo without shell hacks
- tests cover patch/read/search/list basics

### Slice 4: Browser events hardening

Keep Python ownership but normalize events:

```text
browser.connected
browser.reconnected
browser.target_changed
browser.screenshot
browser.artifact
browser.error
```

Exit:

- reconnect invalidation is explicit
- live browser and screenshot artifacts are reliable

### Slice 5: Codex-shaped subagents

Align to v1:

```text
spawn_agent
send_input
wait_agent
close_agent
```

Exit:

- helper exploration no longer floods parent context
- parent receives compact result

### Slice 6: TUI event projection

Render the new event types as product transcript blocks.

Exit:

- TUI shows task, steps, result, browser state, file edits, commands, and subagents from typed events

## Deliberate Simplifications

Keep these out of the initial implementation:

- Codex sandboxing
- permission prompts
- command prefix approval policy
- `spawn_agents_on_csv`
- multi-agent v2 mailbox semantics as the primary surface
- app-server fuzzy-search streaming
- full Codex code-mode tooling
- shell aliases in the default model prompt

## Phase 0 Completion Checklist

- [x] Codex command tools audited
- [x] Codex patch tool audited
- [x] Codex image tool audited
- [x] Codex plan tool audited
- [x] Codex subagent tools audited
- [x] Codex fuzzy file search audited
- [x] Python main branch shell/file tools audited
- [x] Python main branch browser tool audited
- [x] Python main branch session/subagent tools audited
- [x] Rust rewrite current tool surface audited
- [x] Target tool list selected
- [x] Backend ownership selected
- [x] Tools deliberately excluded from default model surface listed

