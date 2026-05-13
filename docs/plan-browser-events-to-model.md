# Plan: stream browser events to the model (compact)

## Goal

When the model runs the `python` tool, append a compact summary of *signal* browser events (since the previous python tool call) to the tool result text the model sees. Fixes "agent uses stale target ID" loops and missed navigations.

Optimize for **tokens**: every event line must be ≤ ~60 chars. The whole block must be skipped when nothing meaningful changed.

## Where to inject

In `crates/browser-use-core/src/lib.rs`, inside `record_python_response_events_inner` (currently around line 3280), build a compact summary from `response.browser_events` and append it to `response.text` *before* `spill_large_text_output` is called. The model already sees `response.text` via `tool_message_from_output_event` — no other plumbing changes needed.

Persist the raw events as today (each as a session event) so the TUI and replay still get the full fidelity; only the *model-visible* tool output gets the compact summary.

## Event filter — what to include

Walk `response.browser_events` (a `Vec<Value>` of `{type, payload}`). Include only these types:

| `type` | Compact form | Notes |
|---|---|---|
| `browser.target_changed` | `> <host+path, trimmed to 50 chars>` | new active page |
| `browser.reconnected` with `stale_object_ids: true` | `! stale` | tells model selectors/IDs are dead |
| `browser.tab_closed` (or target destroyed) | `- <short_id>` | first 6 chars of `target_id` |
| `browser.tab_opened` (or new target created) | `+ <short_id>` | only when not also covered by `target_changed` |
| `browser.dialog_opened` | `? <type> "<msg, ≤40 chars>"` | freezes JS thread |
| `browser.download_started` | `↓ <filename>` | model often misses these |

Drop entirely: `browser.state` (every-call pulse — see closing line below), `browser.live_url`, anything with a `type` not in the table above.

If `Target.targetCreated` / `Target.targetDestroyed` aren't surfaced today as `browser.tab_opened` / `browser.tab_closed`, derive them by diffing the `tabs` set between successive `browser.state` events. That diff is the source of truth.

### Closing state line

After the event lines, append one final line that summarizes ending state — only if it changed since the previous python call:

```
= <tabs_count>t <vw>x<vh>
```

Where:
- `<tabs_count>t` only if it changed (e.g. `3t`)
- `<vw>x<vh>` only if viewport changed (e.g. `1497x770`)
- If neither changed, omit the `=` line entirely

Cache the last emitted state per session in an in-memory map keyed by `session_id`. (Lost on process restart — acceptable; worst case is one redundant `=` line.)

## Output format

Wrap in a single tag block, no extra prose, no leading blank lines:

```
<browser>
> elibrary.ferc.gov/eLibrary/search
- D43508
! stale
= 2t
</browser>
```

Rules:
- One event per line, prefix is a single ASCII glyph (`>`, `+`, `-`, `!`, `?`, `↓`, `=`).
- URLs: strip `https?://`, strip leading `www.`, truncate to 50 chars with `…` suffix.
- Target IDs: first 6 hex chars.
- **Skip the whole block entirely** if there are zero filtered events AND no state change. Do not emit empty `<browser></browser>`.
- Limit total lines to 8. If more, keep the first 7 and append `… +N more`.
- Append to `response.text` with a single `\n` separator. If `response.text` is empty, the block becomes the entire text.

## Implementation sketch

In `crates/browser-use-core/src/lib.rs`:

```rust
fn compact_browser_events(
    events: &[Value],
    last_state: &mut Option<(u64, u64, u64)>, // (tabs, vw, vh)
) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current_state: Option<(u64, u64, u64)> = *last_state;

    for ev in events {
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let p  = ev.get("payload").unwrap_or(&Value::Null);
        match ty {
            "browser.target_changed" => {
                if let Some(url) = p.get("url").and_then(Value::as_str) {
                    lines.push(format!("> {}", shorten_url(url, 50)));
                }
            }
            "browser.reconnected" => {
                if p.get("stale_object_ids").and_then(Value::as_bool) == Some(true) {
                    lines.push("! stale".into());
                }
            }
            "browser.state" => {
                let tabs = p.get("tabs").and_then(Value::as_u64).unwrap_or(0);
                let (vw, vh) = p.get("viewport")
                    .map(|v| (v.get("w").and_then(Value::as_u64).unwrap_or(0),
                              v.get("h").and_then(Value::as_u64).unwrap_or(0)))
                    .unwrap_or((0, 0));
                current_state = Some((tabs, vw, vh));
            }
            // …dialog_opened, download_started, tab_opened/closed from state diff
            _ => {}
        }
    }

    // closing state line: only if changed
    if let Some(s) = current_state {
        if Some(s) != *last_state {
            let (t, vw, vh) = s;
            let changed_tabs = last_state.map_or(true, |ls| ls.0 != t);
            let changed_vp   = last_state.map_or(true, |ls| (ls.1, ls.2) != (vw, vh));
            let parts: Vec<String> = std::iter::empty()
                .chain(changed_tabs.then(|| format!("{t}t")))
                .chain(changed_vp.then(|| format!("{vw}x{vh}")))
                .collect();
            if !parts.is_empty() {
                lines.push(format!("= {}", parts.join(" ")));
            }
            *last_state = current_state;
        }
    }

    if lines.is_empty() { return None; }
    if lines.len() > 8 {
        let extra = lines.len() - 7;
        lines.truncate(7);
        lines.push(format!("… +{extra} more"));
    }
    Some(format!("<browser>\n{}\n</browser>", lines.join("\n")))
}

fn shorten_url(url: &str, max: usize) -> String {
    let s = url.trim_start_matches("https://")
              .trim_start_matches("http://")
              .trim_start_matches("www.");
    if s.chars().count() <= max { return s.into(); }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}
```

Then in `record_python_response_events_inner` (line ~3282), before the `spill_large_text_output` call:

```rust
let mut response_text = response.text.clone();
let last_state = browser_state_cache.entry(session_id.to_string()).or_default();
if let Some(block) = compact_browser_events(&response.browser_events, last_state) {
    if !response_text.is_empty() {
        response_text.push('\n');
    }
    response_text.push_str(&block);
}
let (text, text_artifact) = spill_large_text_output(store, session_id, &response_text)?;
```

Where `browser_state_cache` is a `Mutex<HashMap<String, Option<(u64, u64, u64)>>>` stored alongside other per-session state (similar pattern exists elsewhere — check for `OnceLock<Mutex<HashMap<...>>>` near top of `lib.rs`). If no obvious home, add one as a module-level `OnceLock`.

## Tests

Add unit tests for `compact_browser_events`. Cases:

1. Empty events, no prior state → returns `None`.
2. Empty events, prior state present, current state same → returns `None`.
3. Single `target_changed` to long URL → block with one `> host/path…` line ≤ 53 chars.
4. `reconnected` with `stale_object_ids: true` → `! stale` line.
5. State change: tabs 2→3, viewport unchanged → `= 3t` (no viewport).
6. 12 events → output truncated to 7 lines + `… +5 more`.
7. Two successive calls with the same state → second call emits no `=` line.

No integration tests required for v1 — manual smoke via `dataset-run-openrouter real_v14_short --count 1` and grep for `<browser>` in `events <task_id>`.

## Out of scope (do NOT include in this PR)

- Synthetic user-role messages (codex-style `ContextualUserFragment`). The current approach piggybacks on the python tool result — adequate because browser state only changes via tool calls in this harness.
- New event sources (network failures, console errors). v1 uses only events the worker already collects.
- TUI filtering of the `<browser>` block from the human transcript. The TUI already renders structured `browser.*` events from the session store separately; the inline `<browser>` block will appear inside the python tool output, which is acceptable for v1.
- Per-event rate limiting or throttling. Truncation to 8 lines is sufficient.
- Configurability (no env var or CLI flag for v1). Always on.

## Acceptance

After this lands, run:

```bash
LLM_BROWSER_BROWSER_MODE=cloud BU_NAME=tmp-browser-events-smoke \
  uv run browser-use-terminal --state-dir /tmp/but-browser-events-smoke \
  dataset-run-openrouter real_v14_short --count 1 \
  --model qwen/qwen3.6-plus-04-02 --max-turns 60
```

Then:

```bash
uv run browser-use-terminal --state-dir /tmp/but-browser-events-smoke \
  events <session_id> | grep -A 6 '<browser>' | head -40
```

Expected: `<browser>` blocks visible inside `tool.output` events, ≤ 8 lines each, no empty blocks, URLs trimmed.
