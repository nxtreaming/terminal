# llm-browser

A browser-specific LLM harness built around raw Chrome DevTools Protocol access, durable sessions, editable helpers, and screenshot timelines.

Current status: vertical MVP implementation is underway. The current runtime already has:

- append-only session/event logs
- fake, OpenAI Responses, and Codex Responses provider paths
- redacted Codex auth detection from `~/.codex/auth.json`
- harness-owned Chrome launch through CDP
- persistent Python browser tool with raw `cdp(...)`
- model-visible screenshot tool outputs
- shell and basic file tools
- simple event-driven terminal UI

- `docs/browser-agent-harness-plan.md`
- `docs/browser-agent-harness-learnings.md`
- `docs/implementation-roadmap.md`

## Local Commands

```bash
uv run llm-browser doctor
uv run llm-browser auth status
uv run llm-browser run --provider fake "Open example.com"
uv run llm-browser run --provider codex --model gpt-5.5 "Call the done tool with result ok."
uv run llm-browser browser smoke --headless --url https://example.com
uv run llm-browser tui
uv run llm-browser sessions list
```

By default runtime state is stored under `.llm-browser/`.

For headless browser tool runs:

```bash
LLM_BROWSER_HEADLESS=1 uv run llm-browser run --provider codex --model gpt-5.5 \
  "Use python headless true. Open https://example.com, screenshot('loaded', attach=True), then call done with the title."
```
