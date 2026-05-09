Browser Python quick reference:
- CDP is the source of truth: `cdp("Domain.method", **params)`.
- Browser-level discovery uses `Target.*`; page work usually needs an attached session plus `Page/Runtime/DOM/Network.enable`.
- Helpers are Python functions inside the `python` tool code, not top-level tools.
- Helpers are convenience wrappers: `page_info()`, `list_tabs()`, `current_tab()`, `goto_url(url)`, `navigate(url)`, `new_tab(url)`, `reattach_cdp(...)`.
- Remote-browser recovery helper: `reattach_cdp(target_id=None, url_contains=None)` reconnects the websocket if needed and attaches a fresh page CDP session.
- Inspect with CDP `Runtime.evaluate`/DOM/Page/Network, `js(expr)`, `capture_screenshot(path=None, attach=True)`, and `help_browser()`.
- Interact with CDP input events or helpers: `click_at_xy(x, y)`, `fill_input(selector, text)`, `press_key(key)`, `type_text(text)`, and `scroll()`.
- The harness owns Chrome lifecycle and the active CDP connection; use injected `cdp(...)` instead of raw localhost DevTools URLs or relaunching Chrome.
- agent_helpers.py is editable for task-specific helper functions.
