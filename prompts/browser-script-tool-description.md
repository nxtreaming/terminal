Run Python for browser page interaction through the Rust-held CDP connection.

This is the browser interaction tool and page/data-plane tool. Use it for navigation, page inspection, clicks, typing, scrolling, screenshots, downloads, uploads, network inspection, extraction, browser-backed verification, artifacts, and final answers.

Use the `browser` tool for connection/runtime work first. If the browser is not connected, run `browser status --json` and then an explicit connect command such as `browser connect local`, `browser connect managed --headless`, or `browser remote start`.

Important execution model:

- Each `browser_script` call starts a fresh Python process.
- Python variables do not persist across calls.
- Browser/CDP state persists in Rust.
- Fast calls return their final result immediately. Long calls return `status: running` with a `run_id`; keep observing that same run until it finishes, fails, or is cancelled.
- To listen to a running script, call this tool with `action="observe"`, the returned `run_id`, and optionally `observe_timeout_ms`. If observe reports no new output, wait/back off instead of polling in a tight loop.
- To stop a running script, call this tool with `action="cancel"` and the `run_id`. Partial images and artifacts emitted before cancellation are preserved.
- A failed `browser_script` call may include a short diagnosis. Read that diagnosis first: if it says the browser is still connected or the same page is usable, continue from the same page instead of reconnecting.
- Helpers are preimported; you do not need imports for normal browser work.
- CDP is the source of truth. If a helper is incomplete, use `cdp(...)` directly.
- Keep browser actions sequential and deliberate.
- Do not import Playwright, Selenium, or Pyppeteer.

Preimported helpers:

```python
cdp(method, session_id=None, **params)
cdp_batch(calls)
js(expression_or_function_source, *args, target_id=None, returnByValue=True)

new_tab(url="about:blank")
goto_url(url)
page_info()

capture_screenshot(...)
screenshot(label="screenshot", full=False)
screenshot_clip(label, x, y, width, height)

click_at_xy(x, y)
fill_input(selector, text, clear=True)
type_text(text)
press_key(key, modifiers=0)  # accepts chords like "Meta+A"; modifiers: Alt=1, Ctrl=2, Meta/Cmd=4, Shift=8
scroll(x=0, y=600)

wait_for_load(timeout=10)
wait_for_element(selector, timeout=10)
wait_for_network_idle(timeout=10)

current_tab()
list_tabs()
switch_tab(target_id)
ensure_real_tab()

upload_file(...)
drain_events()
http_get(url, **kwargs)

copy_artifact(path, kind="file")
emit_output(value, label=None)
emit_image(path, label=None)
artifact_root()
outputs_dir()
session_metadata()
audit_artifact(data=None, **requirements)
load_agent_helpers()
agent_workspace()
domain_skills_for_url(url_or_domain, include_content=False)
last_domain_skills(include_content=False)
```

Usage guidance:

- First navigation should usually be `new_tab(url)`, not `goto_url(url)`, because `goto_url(url)` mutates the current controlled tab.
- Keep keyboard semantics browser-harness/Rod aligned: `press_key(...)` simulates physical keys or shortcuts, while `type_text(...)` inserts/pastes text into the focused element with `Input.insertText`.
- For React/Vue/Svelte/controlled inputs, prefer `fill_input(selector, text)` over direct DOM value assignment. It focuses the element, clears with Cmd/Ctrl+A plus Backspace, types through physical key events, then fires final `input`/`change` events.
- Do not combine `Input.dispatchKeyEvent` carrying printable `text` with a manual `char` event for the same character; that double-inserts text in Chrome.
- If the task is site-specific, call `domain_skills_for_url(url, include_content=True)` before inventing selectors, private API routes, or flows. `goto_url(url)` also returns matching `domain_skills` metadata when a skill root is available.
- Use screenshots as labeled temporal checkpoints: initial load, before/after meaningful clicks, scrolls, route changes, dialogs, uploads, downloads, and final verification.
- The common screenshot call is `screenshot(label)`, for example `screenshot("before_submit")`.
- Screenshot/image artifacts are sent as `input_image` content to the next model turn. The user does not see those pixels inline in the terminal; describe what you see or provide the saved artifact path when the user asks for the screenshot.
- If a script emits screenshots/images and then fails, the next model turn still receives the images alongside the failure diagnosis. Use those pixels to decide the next smaller retry.
- If a running script emits screenshots/images before it finishes, `observe` returns those images as soon as they are available. Use the pixels to guide the next observe/retry.
- Use `emit_output(value, label="...")` for structured observations that the next model turn may need, such as `page_info()`, extracted rows, selected DOM state, or API responses. The full value stays model-visible.
- When a script emits labeled structured output, add a `# browser_summary:` JSON comment block at the top of the script that maps each emitted label to the compact transcript summary. Write the code/labels first mentally, then place or update this block before submitting the tool call; the runtime parses the whole script before execution.
- Summary values may be literals, JSONPath-like selectors such as `$.url`, or template strings such as `Read ${$.length} employee rows`. Missing summary specs fall back to a generic `Recorded <label>` summary while preserving the full output.
- Prefer this pattern over printing page or extraction objects:

```python
# browser_summary:
# {
#   "page_info": {
#     "kind": "page",
#     "url": "$.url",
#     "title": "$.title"
#   },
#   "employee_rows": {
#     "kind": "extracted",
#     "message": "Read ${$.length} employee rows"
#   }
# }

info = page_info()
emit_output(info, label="page_info")

rows = [{"name": "Ada"}, {"name": "Grace"}]
emit_output(rows, label="employee_rows")
```

- Keep `print(...)` for short debug/status text only. Do not print large page, DOM, network, or extraction objects when `emit_output(...)` can carry the full value.
- Prefer coordinate clicks for visible UI: screenshot, inspect pixels, `click_at_xy(x, y)`, wait, screenshot again.
- Use `js(...)` for DOM inspection and raw `cdp(...)` for lower-level browser actions.
- Use `js(function_source, *args)` when passing JSON-serializable Python values into JavaScript; use `target_id=` as a keyword for iframe targets.
- For real user forms, act like a browser user: screenshot, click the visible field/control, type with `type_text(...)`, `press_key(...)`, or `fill_input(...)`, then screenshot or otherwise verify. Use coordinate clicks for checkboxes, radios, buttons, dropdowns, and custom controls. Do not assign `element.value`, `element.checked`, `selectedIndex`, React private state, or MutationObserver restore loops on live forms. Do not synthesize `input`, `change`, `click`, or keyboard events in page JavaScript to make a form look filled. Those anti-patterns can desynchronize framework state from the visible DOM.
- Use `http_get(...)` for static pages and APIs after the browser reveals stable endpoints. It returns the response body as a string by default, or bytes with `binary=True`; the returned body also exposes `.status_code`, `.headers`, `.url`, `.text`, `.content`, and `.json()` for convenience. If direct HTTP hits bot or login protection, retry with site-specific headers/cookies, `js(fetch(...))` in the browser, or the configured Browser Use fetch proxy.
- Save complete generated result files under `outputs_dir()` or relative paths in the current working directory. Files written there are collected as artifacts automatically; `copy_artifact(...)` is for files created elsewhere.
- For large structured results, write the full JSON/CSV/text to a file. If the task asks for an exact inline final format, return that content with `done(result=...)` and optionally include `result_file=path`; otherwise finish with `done(result_file=path)`.
- For long extraction or verification loops, prefer bounded chunks with checkpoints written to files. If a chunk fails with a usable-page diagnosis, shrink the next chunk and resume from the last checkpoint.

Do not call runtime-management helpers here. There is no `browser_connect`, `browser_status`, `browser_doctor`, or `browser_recover` helper in this tool. Those are intentionally only in the `browser` tool so the model can reason about browser lifecycle explicitly.
