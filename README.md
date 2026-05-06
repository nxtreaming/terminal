# browser use terminal

A browser-specific LLM harness built around raw Chrome DevTools Protocol access, durable sessions, editable helpers, and screenshot timelines.

Current status: vertical MVP is implemented and being hardened against the bundled datasets. The current runtime has:

- append-only session/event logs
- fake, OpenAI Responses, and Codex Responses provider paths
- harness-owned Codex auth with import, device-code login, refresh, and read-only Codex CLI fallback
- browser backends for owned Chromium/Chrome, Browser Use cloud browsers, explicit CDP endpoints, and real Chrome profile attach
- persistent Python browser tool with raw `cdp(...)`, console/network/download helpers, network-idle waits, dialog tracking, and browser trace export
- model-visible screenshot tool outputs with ordered image timelines
- shell streaming, long-running shell processes, optional PTY processes, cancellation, and Codex-style file tools
- event-driven Textual terminal UI with session detail, artifact table, artifact preview, trace, eval, resume, cancel, auth, and config commands
- background sessions, cancellation, trace replay, resume, and self-eval commands
- dataset sampling/running for `real_v8` and `real_v14_short`
- JSON config defaults for provider, model, browser backend, profile, cloud, and viewport settings

- `docs/browser-agent-harness-plan.md`
- `docs/browser-agent-harness-learnings.md`
- `docs/implementation-roadmap.md`

## Local Commands

```bash
uv run browser-use-terminal doctor
uv run browser-use-terminal auth status
uv run browser-use-terminal auth codex import
uv run browser-use-terminal config show
uv run browser-use-terminal config init
uv run browser-use-terminal run --provider fake "Open example.com"
uv run browser-use-terminal run --provider codex --model gpt-5.5 "Call the done tool with result ok."
uv run browser-use-terminal browser smoke --browser chromium --headless --url https://example.com
uv run browser-use-terminal tui --browser chromium
uv run browser-use-terminal sessions list
uv run browser-use-terminal datasets sample real_v8 --count 1 --seed 21
uv run browser-use-terminal datasets run real_v8 --provider codex --model gpt-5.5 --count 1 --seed 21
uv run browser-use-terminal sessions self-eval <session-id> --provider codex --model gpt-5.5
```

By default runtime state is stored under `.browser-use-terminal/`. Config is loaded from `~/.browser-use-terminal/config.json` and then `./.browser-use-terminal/config.json`; local config overrides global config, and explicit CLI flags override both.

For headless browser tool runs:

```bash
uv run browser-use-terminal run --browser chromium --headless --provider codex --model gpt-5.5 \
  "Use python headless true. Open https://example.com, screenshot('loaded', attach=True), then call done with the title."
```

## Browser Backends

The browser primitive is still raw CDP. The backend only decides where the CDP websocket comes from.

Owned Chromium/Chrome launches an isolated non-default profile, so it works with current Chrome remote-debugging restrictions:

```bash
uv run browser-use-terminal browser smoke --browser chromium --headless --url https://example.com
uv run browser-use-terminal tui --browser chromium --headless --provider codex --model gpt-5.5
```

Real Chrome attaches to your already-running profile. In Chrome, open `chrome://inspect/#remote-debugging`, enable remote debugging for this browser instance, and click Allow if Chrome asks:

```bash
uv run browser-use-terminal tui --browser real --provider codex --model gpt-5.5
uv run browser-use-terminal browser smoke --browser real --url https://example.com
```

Explicit CDP is the raw escape hatch. This also keeps compatibility with browser harness env vars `BU_CDP_URL` and `BU_CDP_WS`:

```bash
/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome \
  --remote-debugging-port=9222 \
  --user-data-dir=/tmp/browser-use-terminal-profile

uv run browser-use-terminal tui --browser cdp --cdp-url http://127.0.0.1:9222
uv run browser-use-terminal tui --browser cdp --cdp-ws ws://127.0.0.1:9222/devtools/browser/<id>
```

Browser Use cloud provisions a browser, attaches to its CDP websocket, records the live URL in runtime output, and stops the cloud browser on close:

```bash
export BROWSER_USE_API_KEY=...
uv run browser-use-terminal browser smoke --browser cloud --cloud-timeout 60 --cloud-proxy-country us
uv run browser-use-terminal tui --browser cloud --cloud-profile-id <uuid> --provider codex --model gpt-5.5
```

Useful shared options: `--browser-width`, `--browser-height`, `--chrome-path`, `--profile-template`, `--keep-profile`, `--cloud-profile-name`, `--cloud-recording`, and `--cloud-custom-proxy-json`.

## Browser Python Helpers

The main browser tool is still an editable Python runtime. Useful built-ins include:

```python
cdp("Runtime.evaluate", {"expression": "document.title", "returnByValue": True})
fill_input("#email", "me@example.com")
wait_for_network_idle()
screenshot("after_click", attach=True)
recent_console()
recent_network_failures()
download_info()
save_browser_trace("checkout")
Path(agent_helpers_path()).write_text("from browser_helpers import *\n\ndef page_title():\n    return js('document.title')\n")
reload_agent_helpers()
```

Downloads land under each session's `browser/downloads/`, screenshots under `browser/screenshots/`, browser traces under `browser/traces/`, and oversized tool output under `tool-output/`.

For interactive terminal programs, use the `shell_start` tool with `pty=true`, then `shell_poll`, `shell_stdin`, and `shell_stop`.

## Recent Verification

```bash
uv run python -m unittest discover -s tests
uv run browser-use-terminal browser smoke --browser chromium --headless --url https://example.com
uv run browser-use-terminal datasets run real_v8 --provider fake --count 1 --seed 3
```

Real `gpt-5.5` dataset runs completed:

- `real_v8` task 34, session `940ce19a2ef4`: Shopify app contact extraction with screenshot artifact.
- `real_v14_short` task 9, session `a4c4517fd58d`: SBI home loan rate table screenshot, verified image output in isolated workspace.
- `real_v8` task 22, session `eedd29928174`: DNA/Telia package extraction after workspace isolation fixes.
- `real_v14_short` full run `real-v14-gpt55-full`: 10 selected, 10 passed. Task 11 required a second latest attempt after the FCC origin stalled; the successful attempt used the `fccid.io` mirror and returned all seven grantee-code counts.

Current long run:

- `real_v8` full run `real-v8-gpt55-full` is in progress with `gpt-5.5`, `--all`, `--resume`, and per-task timeout.
