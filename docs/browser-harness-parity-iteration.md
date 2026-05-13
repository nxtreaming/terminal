# Browser Harness Parity Iteration

This is the working note for making the Rust rewrite behave like the proven Codex + browser-harness setup while keeping the new direct image-output advantage.

## Goal

Replicate browser-harness behavior as the substrate:

- one Python execution surface
- raw CDP as the source of truth
- browser-harness helper signatures and semantics preserved
- coordinate-first interaction
- screenshots as the primary visual feedback loop
- interaction skills as tactical reference docs, not a high-level framework

Then add only additive improvements:

- direct `input_image` output from the Python tool
- ordered screenshot timelines in one tool call
- CDP-native clipped screenshots
- Rust/TUI/session plumbing around the same browser-harness mental model

## Screenshot Timeline Policy

The model should be encouraged to screenshot often, but screenshots must be tied to browser state over time.

Good pattern:

```python
screenshot("initial")
click_at_xy(x, y)
wait_for_network_idle()
screenshot("after_click")
```

For longer flows:

```python
screenshot("loaded_payroll_admin")
click_at_xy(...)
wait_for_network_idle()
screenshot("employee_table_open")
scroll(...)
screenshot("employee_table_after_scroll")
```

Rule: every screenshot should have a purpose: observe current state, verify an action, inspect a changed region, or preserve final evidence. Repeating the same unchanged viewport is a harness failure, not visual memory.

## Screenshot Clip

Clipped screenshots should be CDP-first.

Preferred implementation shape:

```python
cdp(
    "Page.captureScreenshot",
    format="png",
    clip={"x": x, "y": y, "width": width, "height": height, "scale": 1},
)
```

Post-cropping a full screenshot with PIL should only be a fallback or optional post-processing step. CDP clipping is cheaper, faster, and avoids device-pixel coordinate drift. The helper contract should be explicit that clip coordinates are CSS pixels, not raw PNG device pixels.

Possible helpers:

- `screenshot_clip(label, x, y, width, height, attach=True, max_dim=None)`
- `capture_screenshot(..., clip={...})` only if it can be added without breaking browser-harness compatibility
- `screenshot_element(label, selector_or_rect, attach=True)` later, if we need it

## General vs Interaction Tools

Browser-harness has a cognitive split, not a separate model-tool split.

General primitives:

- `cdp`
- `js`
- `page_info`
- `new_tab`
- `goto_url`
- `wait_for_load`
- `wait_for_network_idle`
- `current_tab`
- `list_tabs`
- `switch_tab`
- `ensure_real_tab`
- `drain_events`
- `http_get`
- `copy_artifact`
- `artifact_root`
- `session_metadata`
- `emit_image`
- `capture_screenshot`
- `screenshot`

Interaction primitives:

- `click_at_xy`
- `type_text`
- `press_key`
- `fill_input`
- `scroll`
- `upload_file`
- dialogs
- downloads
- iframes
- shadow DOM
- dropdowns
- profile sync

Recommendation: keep one Python tool and one persistent namespace. Do not split into `python_general` and `python_interaction` model tools. That would add tool-choice overhead and move away from the browser-harness behavior. Instead, split the system prompt/tool description into General and Interaction sections and keep flat helper aliases for compatibility.

Optional runtime nicety:

```python
general.cdp(...)
interaction.click_at_xy(...)
```

Only add that if flat aliases remain first-class.

## What Prevents Exact Parity

Helper-level parity is straightforward. Full behavioral parity is harder because browser-harness behavior comes from the whole stack:

- Codex model priors around shell + `browser-harness -c`
- `SKILL.md`
- `interaction-skills/*.md`
- raw CDP helpers
- the CLI transcript shape
- screenshot files viewed by the model
- daemon/session behavior

The rewrite changes the model-facing surface:

- tool name is `python`, not shell `browser-harness -c`
- function-call output can include images directly
- Rust owns session state, artifacts, TUI state, and provider formatting
- compaction can rewrite or weaken browser context
- `capture_screenshot` currently wraps browser-harness behavior by attaching images

The parity strategy is to preserve the substrate and make the improvements additive.

## Proposed Changes

1. Vendor or load the real browser-harness `SKILL.md` almost verbatim into the browser-agent system prompt.
2. Vendor or load `interaction-skills/*.md` as reference docs.
3. Keep `capture_screenshot` browser-harness-compatible. If direct image output changes semantics, prefer additive helpers like `screenshot(...)` and `emit_image(...)`.
4. Add `screenshot_clip(...)` with CDP `Page.captureScreenshot` `clip`.
5. Split prompt/tool description into General primitives and Interaction primitives.
6. Preserve flat helper aliases.
7. Make compaction preserve the active operating contract, current URL/title, last useful screenshot labels, and the next expected browser action.
8. Keep screenshot guidance temporal: frequent labeled checkpoints, no unchanged screenshot loops.
9. Avoid manager layers, page objects, selector-first wrappers, retry frameworks, and Playwright/Selenium abstractions.

## Real v14 Parallel Run

Run shape:

- dataset: `real_v14_short`
- model/provider: `gpt-5.5` through Codex
- browser mode: Browser Use cloud
- parallelism: 10 local CLI processes, one task per process
- isolation: separate state dirs and `BU_NAME`s under `/tmp/but-v14-parity-*`
- important caveat: the local dataset runner marks a task as passed when the session calls `done`; it is not a judge and does not validate factual correctness.

Observed results:

| Task | Local Status | Duration | Python Calls | Images | What Happened |
| --- | --- | ---: | ---: | ---: | --- |
| 2 | passed | 653s | 34 | 3 | FERC search and document summaries completed. Several recoverable Python/CDP errors, but the final answer was useful. |
| 4 | passed locally, behavior failed | 859s | 23 | 4 | The agent discovered the Bullseye locator API and printed a 680-store JSON, then called `done` with `{"stores":[]}`. This is a final-answer handoff failure. |
| 5 | failed | 818s | 37 | 15 | Long multi-site telecom extraction made progress, then failed with Codex `No tool call found for function call output`. |
| 6 | failed | 66s | 7 | 2 | Codex `cyber_policy` refusal on Facebook Ads Library task. |
| 8 | passed | 816s | 20 | 5 | SSD comparison completed by combining browser inspection and PriceRunner searches. Accuracy still needs a judge. |
| 9 | passed | 87s | 13 | 3 | SBI screenshot task completed and returned an artifact path. Hit an element coordinate issue that a clip helper would reduce. |
| 10 | failed | 58s | 5 | 2 | Codex `cyber_policy` refusal during surgeon directory task. |
| 11 | passed | 139s | 11 | 2 | FCC counts completed efficiently after switching from UI/form attempts to in-page request automation. |
| 13 | passed | 339s | 24 | 1 | WakeMed provider scrape completed by discovering pagination/API shape and using bulk HTTP plus an artifact. |
| 16 | failed | 368s | 26 | 12 | McDonald's menu extraction made substantial progress, then failed with the same Codex unmatched function-call-output error as task 5. |

The screenshot policy did not create a 50-screenshot loop in this run. Hard UI tasks used more screenshots, but they were mostly attached to changed states. The bigger failures were final-data handoff and provider/tool-output protocol stability.

## Generalizable Learnings

### 1. Final Answer Handoff Must Be Deterministic

Task 4 is the clearest failure. The agent did the hard part: it discovered the store locator API, collected 680 stores, formatted JSON, and printed the first chunk. Then it called `done` with an empty result.

This generalizes. Long printed output is not a durable final-answer channel. The harness and prompt should make this rule explicit:

- If a Python call computes final structured data, assign it to `result`, write it to an artifact when large, and print only a short count/summary.
- Do not rely on a huge `print(json.dumps(...))` as the only bridge to the next model turn.
- Before calling `done`, verify the final answer count matches the extracted count.
- For structured-output tasks, never replace non-empty extracted data with an empty schema-shaped fallback unless the extraction count is actually zero.

Potential helper:

```python
set_final_answer(data, artifact_name=None)
```

This should store the data in the Python namespace, optionally save/copy an artifact, emit a short summary, and make the next `done` call easy to construct exactly.

### 2. Provider Tool-Output Protocol Needs Replay Tests

Tasks 5 and 16 both failed with:

```text
No tool call found for function call output with call_id ...
```

That is not a website problem. It is a generalized provider-message serialization problem, likely around long histories, image-bearing tool outputs, or synthetic visual-context messages.

Required general fix:

- Add provider replay tests with many Python calls, image outputs, failed tool calls, and long histories.
- Ensure every `function_call_output` is emitted only when the matching assistant `function_call` is still present in the provider input.
- If images are moved into synthetic visual-context messages, do not drop or reorder the original tool-call/output pair.
- Add a shrink/reproduction test from task 5 or 16 before changing prompt text further.

### 3. Bulk Extraction Should Be A First-Class Browser-Harness Pattern

The best successes did not click through every visible row:

- Task 11 used the page/form shape to automate FCC searches.
- Task 13 discovered WakeMed pagination and switched to HTTP/profile-page scraping.
- Task 2 used browser navigation to discover FERC file URLs, then read PDFs/DOCX directly.

General rule:

After the browser reveals stable data endpoints, static links, XHR/fetch patterns, downloadable assets, or predictable pagination URLs, switch to `requests`, `http_get`, `fetch` inside `js`, or `ThreadPoolExecutor`. Use the browser to discover and verify, not to mechanically click every item when the network/data shape is available.

This belongs in the browser-harness contract because it is not site-specific and it is one of the largest efficiency wins.

### 4. Screenshots Are Good, But They Need Scope Control

The new prompt direction is mostly right. The run used screenshots as checkpoints without degenerating into repeated unchanged screenshots.

Refinement:

- Batch screenshots around UI transitions.
- Avoid mixing slow network scraping, large parsing, and visual interaction into one huge Python call. If a call times out, the model loses the whole batch.
- Use screenshots for visual state, then persist extracted data through artifacts or `result`.
- Add CDP-native `screenshot_clip` for table/region tasks so the model does not have to reason in raw device-pixel screenshots.

### 5. Large Data Should Prefer Artifacts Plus Counts

Task 13 succeeded by returning a concise final object with a JSON artifact path. Task 4 failed after printing huge JSON into the tool transcript.

General rule:

- For hundreds or thousands of rows, save the full JSON/CSV artifact.
- Print and return counts, schema sample, and artifact path.
- If the task requires inline JSON, keep the final `done` value exact, but do not use printed transcript text as the source of truth.

### 6. `js(...)` Return Values Need Tool-Description Clarity

Several recoverable failures came from treating Python values returned by `js(...)` like JavaScript values, for example calling `.slice(...)` on a Python string.

Prompt/tool-description rule:

`js(...)` returns Python data. Use Python slicing and Python methods after the call. If you need JavaScript methods, put them inside the JavaScript expression before returning.

### 7. Eval Runner Needs Judging Or Validation

The local runner counted task 4 as passed despite an obviously wrong empty result. This will hide regressions.

General fixes:

- Add schema validation for structured tasks.
- Add simple output sanity checks when task text implies non-empty data.
- Run an LLM judge over final answers and artifacts for browser datasets.
- Distinguish local completion from judged success in reports.

### 8. Timeout Failures Should Preserve Partial Progress

Multiple tasks had recoverable Python tool timeouts. Browser-harness style encourages large Python snippets, which is good, but a timeout can discard important work unless the code checkpoints data.

General rule:

- For long extraction loops, write incremental artifacts/checkpoints.
- Keep visual interaction calls short.
- Put bulk scraping in resumable helper functions that save progress as they go.
- Prefer many durable chunks over one giant uncheckpointed extraction call.

## Open Questions

- Should `capture_screenshot(..., attach=True)` remain the default, or should parity require `capture_screenshot` to return only a path and make `screenshot(...)` the image-attaching helper?
- Should the prompt literally embed browser-harness `SKILL.md`, or should we generate a distilled browser-agent contract from it?
- Should interaction skills be always included, lazily loaded, or surfaced through a Python helper such as `interaction_help("dropdowns")`?
- How much of the shell transcript shape can or should we mimic in first-class tool outputs?
- What should compaction store about screenshots: last label only, labels plus visual summaries, or selected image references?

## Not Generalizable

These should not become broad harness rules yet:

- The specific Bullseye Locations API parameters used for Ollie's.
- The specific FERC `filedownload?fileid=...` document path and docket-table behavior.
- The specific WakeMed provider pagination/profile URL structure.
- The specific FCC form field names and result-count phrasing.
- The specific McDonald's/DoorDash DOM text layout.
- The specific PriceRunner product matching shortcuts used for Danish SSDs.
- The specific Samlino/Yousee/Norlys/3.dk selectors and cookie flows.
- Codex `cyber_policy` refusals on tasks 6 and 10 as browser-harness evidence. Track them as provider/eval blockers, not as proof that browser interaction behavior is wrong.
