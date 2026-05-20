Browser runtime control tool.

This tool is the browser control plane. It manages which browser is connected, who owns it, how CDP is attached, what recovery is safe, and what the current runtime knows. It does not click, type, scrape, screenshot, run page JavaScript, or inspect pixels. Use `browser_script` for page interaction.

The input is a single CLI-like command string. You may include the leading word `browser`, but it is optional:

```text
browser status --json
browser connect local
browser local list --json
browser local setup
browser connect managed --headed
browser remote start --profile-name Work
browser recover reconnect-websocket
```

Mental model:

- `browser` owns runtime/control/debug.
- `browser_script` owns page interaction/data extraction.
- Rust holds the CDP websocket, current target id, current session id, ownership, and connection generation.
- Python in `browser_script` is fresh per call; Python variables do not persist.
- Nothing reloads, relaunches, closes, or switches tabs silently. If IDs may change, this tool reports that and you choose the next action.

Local real browser:

- `browser connect local` attaches to an already-running Chromium-family browser after the user enables remote debugging.
- Do not guess a browser family flag. The tool auto-detects Chrome, Chrome Canary, Chromium, Edge, Brave, Arc, Dia, Comet, and common forks through DevToolsActivePort.
- If one candidate exists, it connects. If multiple candidates exist, ask the user which candidate to use, then run `browser connect local --candidate <id>`.
- If Chrome blocks the connection, run `browser local setup`. The user must enable `chrome://inspect/#remote-debugging` and accept any Chrome permission prompt. Then run `browser connect local` again.
- Do not launch the user's real default Chrome profile with remote-debugging flags. Real logged-in profiles are attached while already open.

Managed browser:

- `browser connect managed` starts a Rust-owned browser with a temp profile by default.
- Use `--headless` or `--headed`; default is headless.
- Use `--profile <path>` only for an explicit non-default automation profile.
- Rust may stop/restart this browser because Rust owns it. It is not the user's real logged-in Chrome.

Remote browsers:

- `browser connect remote-cdp --url <http-url>` attaches to an external DevTools HTTP endpoint.
- `browser connect remote-cdp --ws <ws-url>` attaches to an external CDP websocket.
- `browser remote start ...` creates a Browser Use cloud browser and connects to it. Remote start means start and connect; do not copy the returned CDP URL into another command.
- `browser remote stop` only stops a Browser Use cloud browser created by this runtime.
- `browser remote profiles --json` lists cloud profiles without raw cookie values.

Doctor:

- `browser doctor` and `browser doctor --json` are read-only.
- Doctor checks runtime state, local browser candidates, profile-use, API key, CDP websocket health, current target health, and safe next steps.
- Doctor never fixes state by itself. If a fix is available it prints an explicit command.

Recovery:

- `browser recover reconnect-websocket`: reconnects the CDP websocket to the same endpoint. It never reloads the page.
- `browser recover reattach-same-target`: attaches a fresh CDP session to the same target id. If the target is gone, it reports available targets and does not silently switch.
- `browser recover restart-runtime`: resets the Rust connection holder and reconnects to the same endpoint. It does not kill Chrome.
- `browser recover restart-owned-browser`: restarts only Rust-owned managed browsers.
- `browser recover stop-owned-remote`: stops only Rust-owned Browser Use cloud browsers.

Commands:

```text
browser help
browser status --json
browser doctor
browser doctor --json

browser connect local
browser connect local --candidate <id>
browser connect managed [--headless|--headed] [--profile temp|<path>] [--arg <chrome-arg>...]
browser connect remote-cdp --url <http-url>
browser connect remote-cdp --ws <ws-url>

browser local list --json
browser local setup
browser local profiles --json
browser local profiles inspect <profile-name> --domains-only

browser remote start [--profile-id <uuid>|--profile-name <name>] [--timeout <minutes>] [--proxy-country <iso2|none>]
browser remote stop
browser remote status --json
browser remote live-url
browser remote profiles --json

browser recover reconnect-websocket
browser recover reattach-same-target
browser recover restart-runtime
browser recover restart-owned-browser
browser recover stop-owned-remote

browser runtime logs
browser runtime ownership --json
browser runtime cleanup-stale
```

Use `browser status --json` before recovery when the situation is unclear. Use `browser runtime ownership --json` before stopping anything. External user Chrome is never killed or relaunched by this tool.
