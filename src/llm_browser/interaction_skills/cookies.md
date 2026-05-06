# Cookies And Consent

Use `load_skill("cookies")` for browser cookies/storage/permissions. Use `load_skill("cookie_banners")` only when a visible consent UI blocks the page.

```python
load_skill("cookie_banners")
dismiss_cookie_banners(prefer="accept")
capture_screenshot()
```

Consent banners are site/vendor recipes, not core browser control. If the helper misses, inspect the screenshot and use coordinate clicks or write a task-specific helper in `agent_helpers.py`.
