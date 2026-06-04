# Connection & Tab Visibility

## The omnibox popup problem

When Chrome opens fresh, the only CDP `type: "page"` targets can be `chrome://inspect` and `chrome://omnibox-popup.top-chrome/` (a 1px invisible viewport). If the runtime attaches to the omnibox popup, all subsequent work, including `new_tab()` and `goto_url()`, can happen on tabs that exist in CDP but may not be visible in the Chrome UI.

Rust's browser runtime avoids internal targets when attaching and creates an `about:blank` tab when no real pages exist. If you still end up on an invisible tab, use `ensure_real_tab()` or `switch_tab(target_id)` from `browser_script`.

## Startup sequence

1. Check connection state with `browser status --json`
2. If not connected, use the selected browser mode's explicit command, such as `browser connect local`, `browser connect managed --headless`, or `browser remote start`
3. If the connection is stale, use `browser recover reconnect-websocket` or `browser recover reattach-same-target`
4. In `browser_script`, list open tabs with `list_tabs()` to see what's available
5. `ensure_real_tab()` or `switch_tab(target_id)` attaches to a real page

```python
tabs = list_tabs()
for t in tabs:
    print(t["url"][:60])

tab = ensure_real_tab()
```

## Bringing Chrome to front

If an external local Chrome is behind other windows or on another desktop:

```python
import subprocess
subprocess.run(["osascript", "-e", 'tell application "Google Chrome" to activate'])
```

## Navigating

Prefer navigating an existing tab over `new_tab()` unless the task needs a new tab. Tabs created via CDP's `Target.createTarget` can open behind the active tab.

```python
tab = ensure_real_tab()
goto_url("https://example.com")
print(page_info())
```

`goto_url(url)` and `new_tab(url)` have zero implicit wait: they send the CDP navigation command and then return without waiting for readyState, network idle, selectors, paint, or sleeps.
If you chain more work in the same script after navigation, explicitly wait or poll before reading/clicking.
If navigation is the last action before yielding to the model, the LLM call itself may provide enough elapsed time; the next call must still inspect state before assuming the page loaded.
