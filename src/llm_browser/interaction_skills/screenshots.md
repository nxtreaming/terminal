# Screenshots

Use screenshots as the default observation loop.

```python
new_tab("https://example.com")
wait_for_load()
capture_screenshot()
click_at_xy(410, 520)
wait_for_network_idle()
capture_screenshot()
```

Prefer visible verification over assuming a DOM action worked. `capture_screenshot(path=None, attach=True)` returns the image to the model and, when `path` is provided, also saves a named file.
