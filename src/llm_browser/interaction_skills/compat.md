# Compatibility

The default browser surface intentionally shows one primary helper per concept. A few aliases remain available for old snippets and migrated helper files, but they are not the preferred first choice.

Prefer these names:

```python
goto_url("https://example.com")
capture_screenshot()
click_at_xy(410, 520)
press_key("Enter")
list_tabs()
wait_for_element("#submit")
```

Compatibility aliases:

```python
navigate(...)          # prefer goto_url(...)
screenshot(...)        # prefer capture_screenshot(...)
click_at(...)          # prefer click_at_xy(...)
press(...)             # prefer press_key(...)
tabs(...)              # prefer list_tabs(...)
wait_for_selector(...) # prefer wait_for_element(...)
wait_for_text(...)
iframe_target(...)
```

Use aliases when adapting existing code. For new task-specific helpers, use the primary names so `from browser_helpers import *` stays small and predictable.
