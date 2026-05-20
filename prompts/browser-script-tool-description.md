Run Python for browser page interaction through the Rust-held CDP connection.

This is the browser interaction tool and page/data-plane tool. Use it for navigation, page inspection, clicks, typing, scrolling, screenshots, downloads, uploads, network inspection, extraction, browser-backed verification, artifacts, and final answers.

Use the `browser` tool for connection/runtime work first. If the browser is not connected, run `browser status --json` and then an explicit connect command such as `browser connect local`, `browser connect managed --headless`, or `browser remote start`.

Important execution model:

- Each `browser_script` call starts a fresh Python process.
- Python variables do not persist across calls.
- Browser/CDP state persists in Rust.
- Helpers are preimported; you do not need imports for normal browser work.
- CDP is the source of truth. If a helper is incomplete, use `cdp(...)` directly.
- Keep browser actions sequential and deliberate.
- Do not import Playwright, Selenium, or Pyppeteer.

Preimported helpers:

```python
cdp(method, session_id=None, **params)
cdp_batch(calls)
js(expression, returnByValue=True)

new_tab(url="about:blank")
goto_url(url)
page_info()

capture_screenshot(...)
screenshot(label="screenshot", full=False)
screenshot_clip(label, x, y, width, height)

click_at_xy(x, y)
fill_input(selector, text, clear=True)
type_text(text)
press_key(key)
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
emit_image(path, label=None)
set_final_answer(data, artifact_name=None, audit=None)
audit_artifact(data=None, **requirements)
load_agent_helpers()
agent_workspace()
```

Usage guidance:

- `goto_url(url)` navigates the current controlled tab. Use `new_tab(url)` only when you intentionally want another tab.
- Use screenshots as labeled temporal checkpoints: initial load, before/after meaningful clicks, scrolls, route changes, dialogs, uploads, downloads, and final verification.
- The common screenshot call is `screenshot(label)`, for example `screenshot("before_submit")`.
- Prefer coordinate clicks for visible UI: screenshot, inspect pixels, `click_at_xy(x, y)`, wait, screenshot again.
- Use `js(...)` for DOM inspection and raw `cdp(...)` for lower-level browser actions.
- Use `set_final_answer(...)` for structured/large results, then finish with `done(use_final_answer=true)` or `done(result="__use_final_answer__")`.
- Save complete generated files under the task artifact/output directory via `copy_artifact(...)` or `set_final_answer(..., artifact_name=...)`.

Do not call runtime-management helpers here. There is no `browser_connect`, `browser_status`, `browser_doctor`, or `browser_recover` helper in this tool. Those are intentionally only in the `browser` tool so the model can reason about browser lifecycle explicitly.
