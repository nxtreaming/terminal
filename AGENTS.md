# Agent Notes

## TUI Verification Loop

Use the deterministic Textual test harness for visual iteration. It avoids relying on a real terminal emulator and can save SVG screenshots from the same widget tree users see.

Recommended loop:

1. Run focused tests after any TUI input, layout, or rendering change:

   ```bash
   uv run --with pytest python -m pytest tests/test_tui.py -q
   ```

2. Run the browser-tool tests when changing browser events, live preview, reconnects, screenshots, or artifact display:

   ```bash
   uv run --with pytest python -m pytest tests/test_tui.py tests/test_python_browser_tool.py -q
   ```

3. Generate screenshots with `BrowserUseTerminalApp.run_test(...)` and `app.save_screenshot(...)`. Keep outputs under `/tmp/but-design-loop/`:

   ```bash
   uv run python - <<'PY'
   import asyncio
   from pathlib import Path
   from llm_browser.session import SessionStore
   from llm_browser.tui.app import BrowserUseTerminalApp

   async def main():
       root = Path("/tmp/but-tui-snapshot-state")
       store = SessionStore(root)
       session = store.create(cwd=Path.cwd())
       store.emit(session.id, "session.input", {"text": "go to google flights"})
       store.emit(session.id, "tool.started", {"name": "python", "arguments": {"code": "goto_url('https://www.google.com/travel/flights')"}})
       store.emit(session.id, "browser.live_url", {"live_url": "https://live.browser-use.com/?wss=example"})
       store.emit(session.id, "session.followup", {"text": "now find a one way flight from ljubljana to zurich"})

       app = BrowserUseTerminalApp(store, provider_label="codex", model_label="gpt-5.5")
       async with app.run_test(size=(150, 44)) as pilot:
           app.selected_session_id = session.id
           app._load_session_log(session.id)
           await pilot.pause()
           app.save_screenshot(filename="tui-session.svg", path="/tmp/but-design-loop")

   asyncio.run(main())
   PY
   ```

4. For manual terminal behavior, run the app in a PTY and send keys with the harness:

   ```bash
   uv run but --provider fake --browser chromium
   ```

   Useful keys to verify: `tab`, `f1`, `ctrl+p`, `/settings`, `pageup`, `pagedown`, arrow keys with an empty composer, `shift+enter`, `ctrl+c` twice, and `ctrl+q`.

5. Mouse policy matters:

   - Outside tmux, Textual mouse reporting is disabled by default so terminal text selection/copy works.
   - Inside tmux, Textual mouse reporting is enabled by default so wheel scrolling reaches the app.
   - Override with `LLM_BROWSER_TUI_MOUSE=0` or `LLM_BROWSER_TUI_MOUSE=1` when testing either mode.

Before calling the TUI polished, inspect screenshots at home, loaded-session, running-session-with-live-preview, command palette, session palette, settings, artifacts, and multiline composer states.
