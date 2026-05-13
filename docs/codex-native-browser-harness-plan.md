# Codex-Native Browser Use Plan

## Super High Level

We want the Rust rewrite to become a Codex-native Browser Use agent with a browser-harness-simple browser layer.

That means:

- Keep the clean Rust and Codex-style runtime.
- Keep browser control thin: raw CDP, small helpers, screenshots, interaction skills, and editable `agent_helpers.py`.
- Bring back main's best performance heuristics only where they belong: agent loop, compaction, prompts and skills, tool scheduling, provider handling, and final answer handling.
- Do not rebuild main's old Python BrowserRuntime.

## Plan

1. **Codex-Native Tool Runtime**

   Add a real parallel/sequential tool scheduler:

   - Parallel-safe tools run concurrently.
   - Stateful or mutating tools serialize.
   - Tool outputs are returned to the model in the original call order.
   - The model-visible tool surface stays Codex-like.

2. **Recoverable Tool Errors**

   Tool failures should usually return model-visible failed tool outputs, not kill the whole session.

   Fatal session errors should be reserved for corrupted state, store failures, impossible runtime states, or provider failures.

3. **Unified Exec Parity**

   Upgrade `exec_command` and `write_stdin` toward Codex unified exec behavior:

   - Better defaults.
   - Yield bounds.
   - Process cleanup.
   - Termination and cancellation.
   - Output metadata.
   - Session shutdown cleanup.

4. **Context Overflow Recovery**

   On context overflow:

   - Compact immediately.
   - Retry once.
   - Fail only if the compacted retry also fails.

5. **Richer Compaction**

   Keep the current deterministic event projection, but preserve more useful memory:

   - Original task.
   - Browser state.
   - Recent errors.
   - Important URLs, paths, and refs.
   - Recent useful tool results.
   - Final-answer and artifact metadata.
   - Last meaningful user followups.

6. **Prompt And Skill Heuristics**

   Bring back main's best eval and browser heuristics as prompts and skills, not runtime complexity:

   - Cookie banners.
   - Sitemap/API-first discovery.
   - Bounded crawling.
   - Bulk HTTP after endpoint discovery.
   - Auth-wall behavior.
   - Avoiding excessive validation loops.
   - Output and artifact discipline.

7. **Browser-Harness Alignment**

   Keep the browser layer thin and aligned with `/Users/greg/Developer/browser-harness`:

   - Raw CDP is the source of truth.
   - Helpers stay small.
   - Screenshots and coordinate clicks come first.
   - Interaction skills are markdown playbooks.
   - Task-specific code belongs in `.browser-use/agent-workspace/agent_helpers.py`.
   - No new browser runtime, page-object layer, session manager, or browser retry framework.

8. **Browser Worker Adapter Hardening**

   Keep worker additions only where they bridge browser-harness to the agent:

   - Direct image attachment.
   - `set_final_answer(...)`.
   - Artifact copying.
   - Browser state events.
   - Better CDP error hints.
   - Thin compatibility wrappers.

   Anything generally useful should be upstreamed to `browser-harness` instead of growing a parallel browser framework here.

9. **Final Answer / Artifact Contract**

   Harden the current `set_final_answer(...)` flow:

   - Preserve final answers through compaction.
   - Make `done(use_final_answer=true)` obvious and reliable.
   - Handle large extracted results without giant printed JSON.
   - Add tests around final answer persistence.

10. **Provider-Specific Wins**

    Add provider-specific features behind capability gates:

    - Codex/provider remote compaction.
    - Anthropic adaptive thinking if still supported.
    - Provider-specific model/request options only where appropriate.

11. **True Async Subagents**

    Eventually make subagents Codex-like:

    - Spawn returns immediately.
    - Child runs in the background.
    - Parent keeps working.
    - Completion notifications/mailbox.
    - `wait_agent` waits for updates.
    - `close_agent` closes descendants.
    - Concurrency limits.

## Suggested Order

1. Recoverable tool errors.
2. Parallel/sequential tool scheduler.
3. Unified exec parity.
4. Context overflow compact-and-retry.
5. Richer compaction.
6. Prompt and skill heuristics.
7. Browser-harness alignment cleanup.
8. Browser worker adapter hardening.
9. Final answer/artifact hardening.
10. Provider-specific compaction/thinking.
11. True async subagents.

## Definition Of Done

The branch should feel mechanically Codex-native while keeping browser control as simple as browser-harness. It should match or beat `main` on eval reliability without reintroducing main's old browser runtime complexity.

For implementation phases, run the standard verification relevant to the touched area:

- `cargo fmt --check`
- `cargo test`
- `uv run --with pytest python -m pytest -q`
- `scripts/verify-terminal-ui.sh` for TUI-impacting work

## Non-Goals

- Do not restore the old Textual TUI.
- Do not rebuild the old Python BrowserRuntime.
- Do not add a browser page-object framework.
- Do not add a second browser session manager around browser-harness.
- Do not bring back main's old Python tool surface wholesale.
