# Plan: browser execution performance and reliability

## Goal

Improve `llm-browser` performance by adopting the BrowserCode ideas that matter architecturally, without assuming JavaScript is faster than Python.

The important differences to port are:

- persistent browser/session ownership
- raw CDP as the normal control surface
- automatic screenshot ingestion at the CDP layer
- compact model-visible recovery hints
- reusable agent scripts
- benchmark/reporting fixes that make performance comparisons trustworthy

This plan intentionally does **not** add more helper wrappers for basic browser actions like page navigation, JavaScript evaluation, or mouse dispatch. Those should be plain CDP calls.

## Priority order

1. Auto-attach raw `Page.captureScreenshot` results.
2. Make the browser prompt/tool docs raw-CDP-first.
3. Add low-token error hints and `page_info` fallback fixes from the v14 plan.
4. Add reusable agent scripts/workspace.
5. Improve provider retries and cost accounting for benchmarks.
6. Later: move long-lived CDP session ownership into Rust.

## 1. Auto-attach native CDP screenshots

**Why:** BrowserCode gets a lot of value from attaching screenshots whenever the model captures pixels. Our current Python tool already attaches screenshots when the model uses `screenshot()`, `screenshot_clip()`, or `capture_screenshot(... attach=True)`, but raw `cdp("Page.captureScreenshot", ...)` does not automatically become an image attachment.

**Change:**

- Wrap the Python-visible `cdp(...)` function in `python/llm_browser_worker/worker.py`.
- When `method == "Page.captureScreenshot"` and the result contains base64 `data`:
  - decode it
  - write it under the session artifact image directory
  - call the existing `emit_image(...)`
  - keep the original CDP return value intact
- Avoid duplicate attachment when existing helpers call `Page.captureScreenshot` internally.
  - Prefer a small suppression/context flag around helper-internal CDP calls.
  - Do not remove the ergonomic `screenshot(label)` helper; it is still useful for named artifacts.

**Files:**

- `python/llm_browser_worker/worker.py`
- `python/tests/test_worker_package.py`
- maybe `prompts/python-tool-description.md`

**Acceptance:**

- A raw call like `cdp("Page.captureScreenshot", format="png")` causes the next tool result to include an image attachment.
- `screenshot()` and `screenshot_clip()` still attach exactly one image.
- Existing Rust image replay still turns the Python response image into `input_image` for the next model turn.

## 2. Make raw CDP the default browser control surface

**Why:** We do not need helper functions for basic browser operations. Helpers are useful when they add host-side behavior: artifacts, target recovery, file upload, downloads, cross-origin target selection, final answer handling, and browser state. Basic CDP methods should stay visible and direct.

**Change:**

- Update `prompts/python-tool-description.md` and browser agent instructions to show raw CDP examples first:
  - `Page.navigate`
  - `Runtime.evaluate`
  - `Input.dispatchMouseEvent`
  - `Input.insertText`
  - `Page.captureScreenshot`
- Keep helper docs short and position helpers as host conveniences, not the primary browser API.
- Remove or deemphasize prompt examples that teach wrappers for simple CDP calls.

**Acceptance:**

- The model sees concise raw CDP examples for navigation, evaluation, input, and screenshot capture.
- Prompt size does not grow.
- The docs explicitly say screenshots from raw `Page.captureScreenshot` are attached automatically.

## 3. Add model-visible recovery hints

**Why:** The v14 plan's error hints make sense because they fire only after failure. They improve recovery without permanent prompt bloat.

**Change:**

- Add `_annotate_error(...)` in `python/llm_browser_worker/worker.py`.
- Append one-line hints for known browser/tool failure signatures:
  - jQuery `:contains` used as CSS
  - `Identifier 'x' has already been declared`
  - cross-origin iframe DOM access
  - stale/closed CDP target id
  - nonexistent `Runtime.getExecutionContexts`

**Acceptance:**

- The original error string remains intact.
- Matching failures append exactly one `Hint: ...` line.
- Unit tests cover each hint pattern.

## 4. Fix `page_info` target fallback

**Why:** `page_info()` is a useful host-side helper because it summarizes browser state. The current CDP fallback can return viewport information without URL/title after a tab or target swap.

**Change:**

- In `_page_info_cdp_fallback(...)`, keep the cached `current_tab()` fast path.
- If URL or title is missing, call `Target.getTargets`.
- Select the attached page target and fill missing URL/title from CDP.
- Keep existing viewport fallback via `Page.getLayoutMetrics`.

**Acceptance:**

- After `new_tab("https://example.com")`, `page_info()` returns a URL and title instead of only viewport fields.

## 5. Add reusable agent scripts/workspace

**Why:** BrowserCode's reusable script workspace is a good idea. It lets the model build small task-specific helpers once and reuse them across tool calls instead of retyping long snippets.

**Change:**

- Standardize a repo-owned workspace path, for example `.browser-use/agent-workspace/`.
- Allow an `agent_helpers.py` or equivalent script module to be loaded into the Python browser namespace.
- Document the pattern in the Python tool description:
  - write reusable helpers there
  - keep generated scripts task-local
  - do not hide important final answer logic in opaque scripts
- Prefer this over adding many built-in helpers.

**Acceptance:**

- A helper written into the workspace can be imported or auto-loaded by later Python tool calls.
- The model can reuse selectors, parsing routines, and domain-specific extraction logic without prompt churn.

## 6. Improve benchmark retry behavior

**Why:** This does not make a single successful run faster, but it makes benchmark results less noisy and avoids treating one provider flake as an agent failure.

**Change:**

- Keep fake dataset runs at `--max-attempts=1`.
- Change real-provider dataset defaults to `--max-attempts=2`:
  - OpenAI
  - Codex
  - Anthropic
  - OpenRouter
- Reuse existing transient failure detection.
- Add a precise permanent-error stop list:
  - no image-capable endpoint
  - context length exceeded
  - deterministic tool schema rejection
  - auth failures

**Do not:**

- Broadly classify every `400 Bad Request` as permanent without provider context.
- Retry errors that will deterministically recur on the next attempt.

**Acceptance:**

- A transient stream/provider failure gets one retry by default.
- Permanent errors stop immediately and are marked clearly in the manifest/retry history.

## 7. Fix cost accounting for evals

**Why:** Cost reporting affects benchmark interpretation. A silent `$0.00` when pricing is unknown is misleading.

**Change:**

- For OpenRouter/OpenAI-compatible chat, request provider usage when supported.
- Parse native usage cost fields, especially OpenRouter `usage.cost`.
- Preserve native cost if present before falling back to local pricing tables.
- Track whether cost is known, estimated, or missing.
- Report missing cost as unknown instead of `$0.00`.

**Acceptance:**

- OpenRouter runs report native provider cost when available.
- Dataset summaries distinguish zero cost from unknown cost.

## 8. Cloud daemon shutdown after dataset runs

**Why:** Useful operational cleanup for cloud benchmark runs, but not core browser-agent performance.

**Change:**

- Stop the Browser Use cloud daemon when a dataset run finishes or is interrupted.
- Only stop the daemon if this run started it.
- Do not stop a user-provided existing daemon/session.

**Acceptance:**

- Cloud benchmark runs do not keep billing after completion.
- Existing external browser sessions are not accidentally stopped.

## 9. Later: Rust-owned long-lived CDP session

**Why:** This is the biggest architectural step. BrowserCode's browser session lives with the agent process and CDP session store, rather than being treated as incidental helper state. Rust should eventually own the durable browser/session lifecycle.

**Shape:**

- Rust owns CDP connection/session/target state.
- Python becomes a scripting client that sends CDP calls through Rust.
- Rust provides explicit controls:
  - reconnect
  - reset browser
  - reset target/session
  - list/switch targets
  - capture screenshot attachment
- Browser events and screenshots become first-class Rust-side artifacts.

**Why later:**

- It touches the runtime boundary.
- It should be done after raw CDP screenshot attachment proves the model behavior improvement.
- It should be benchmarked separately from prompt and retry changes.

## Defer or skip

### Guarded IIFE wrapping for `js()`

Do not prioritize this now. Since the direction is raw-CDP-first, `js()` should not become the main abstraction. Start with error hints. If repeated declaration failures still show up in traces, add the safer catch-and-retry variant in browser-harness:

```python
try:
    return _runtime_evaluate(code)
except RuntimeError as exc:
    if "already been declared" in str(exc):
        return _runtime_evaluate(f"(()=>{{ {code} }})()")
    raise
```

Avoid heuristic pre-wrapping based on regex unless there is strong evidence it is needed.

### More wrappers for navigation/evaluate/mouse

Do not add these. Use raw CDP:

- `Page.navigate`
- `Runtime.evaluate`
- `Input.dispatchMouseEvent`
- `Input.insertText`

Helpers should exist only when they add behavior that raw CDP does not provide by itself.

## Suggested PR split

1. Raw CDP screenshot auto-attach + tests.
2. Prompt/tool docs raw-CDP-first.
3. Error hints + `page_info` fallback.
4. Agent workspace/reusable scripts docs and loader behavior.
5. Dataset retry defaults + permanent-error classification.
6. Native cost accounting.
7. Cloud daemon shutdown ownership.
8. Rust-owned CDP session design/implementation.
