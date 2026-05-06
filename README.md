# browser use terminal

A browser-specific LLM harness built around raw Chrome DevTools Protocol access, durable sessions, editable helpers, and screenshot timelines.

Current status: vertical MVP is implemented and being hardened against the bundled datasets. The current runtime has:

- append-only session/event logs
- fake, OpenAI Responses, and Codex Responses provider paths
- redacted Codex auth detection from `~/.codex/auth.json`
- harness-owned Chrome launch through CDP
- persistent Python browser tool with raw `cdp(...)`
- model-visible screenshot tool outputs with ordered image timelines
- shell streaming, shell cancellation, and basic file tools
- event-driven Textual terminal UI with session detail, artifact table, artifact preview, trace, eval, resume, and cancel commands
- background sessions, cancellation, trace replay, resume, and self-eval commands
- dataset sampling/running for `real_v8` and `real_v14_short`

- `docs/browser-agent-harness-plan.md`
- `docs/browser-agent-harness-learnings.md`
- `docs/implementation-roadmap.md`

## Local Commands

```bash
uv run browser-use-terminal doctor
uv run browser-use-terminal auth status
uv run browser-use-terminal run --provider fake "Open example.com"
uv run browser-use-terminal run --provider codex --model gpt-5.5 "Call the done tool with result ok."
uv run browser-use-terminal browser smoke --headless --url https://example.com
uv run browser-use-terminal tui
uv run browser-use-terminal sessions list
uv run browser-use-terminal datasets sample real_v8 --count 1 --seed 21
uv run browser-use-terminal datasets run real_v8 --provider codex --model gpt-5.5 --count 1 --seed 21
uv run browser-use-terminal sessions self-eval <session-id> --provider codex --model gpt-5.5
```

By default runtime state is stored under `.browser-use-terminal/`.

For headless browser tool runs:

```bash
LLM_BROWSER_HEADLESS=1 uv run browser-use-terminal run --provider codex --model gpt-5.5 \
  "Use python headless true. Open https://example.com, screenshot('loaded', attach=True), then call done with the title."
```

## Recent Verification

```bash
uv run python -m unittest discover -s tests
uv run browser-use-terminal browser smoke --headless --url https://example.com
uv run browser-use-terminal datasets run real_v8 --provider fake --count 1 --seed 3
```

Real `gpt-5.5` dataset runs completed:

- `real_v8` task 34, session `940ce19a2ef4`: Shopify app contact extraction with screenshot artifact.
- `real_v14_short` task 9, session `a4c4517fd58d`: SBI home loan rate table screenshot, verified image output in isolated workspace.
- `real_v8` task 22, session `eedd29928174`: DNA/Telia package extraction after workspace isolation fixes.
- `real_v14_short` full run `real-v14-gpt55-full`: 10 selected, 10 passed. Task 11 required a second latest attempt after the FCC origin stalled; the successful attempt used the `fccid.io` mirror and returned all seven grantee-code counts.

Current long run:

- `real_v8` full run `real-v8-gpt55-full` is in progress with `gpt-5.5`, `--all`, `--resume`, and per-task timeout.
