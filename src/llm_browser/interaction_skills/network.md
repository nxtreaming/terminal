# Network

Use `wait_for_network_idle()` after navigation, form submit, or actions that trigger XHR/fetch work.

```python
goto_url("https://example.com/search")
wait_for_network_idle(timeout=10, idle_ms=500)
capture_screenshot()
```

Load tracing helpers for recent network diagnostics:

```python
load_skill("tracing")
recent_network_failures()
save_browser_trace("checkout")
```
