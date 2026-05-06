# Waiting

Prefer visual progress checks over long waits.

```python
goto_url("https://example.com")
wait_for_load()
capture_screenshot()
```

Use `wait_for_element(selector, visible=True)` when you need a selector-gated wait. Use `wait_for_network_idle(timeout=10, idle_ms=500)` after actions that trigger fetch/XHR work.

Compatibility helpers are still available when useful:

```python
wait_for_selector("#submit", visible=True)
wait_for_text("Order complete")
```

If you repeat an action, measure progress after each iteration and stop after 1-2 stale iterations.
