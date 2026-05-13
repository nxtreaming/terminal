# Plan: fixes derived from the v14 / Qwen 3.6 Plus run

This plan covers six concrete bugs/improvements surfaced while running `real_v14_short` on `qwen/qwen3.6-plus-04-02` via OpenRouter on 2026-05-11. Browser-event streaming (#10 in the original list) is intentionally **out of scope** and tracked separately in `plan-browser-events-to-model.md`.

Order is roughly increasing complexity. Each item is self-contained and can be PR'd separately.

---

## 1. Error-hint enrichment in tool failure text  (#9)

**Goal:** when the python tool fails with one of a handful of well-known error signatures, append a one-line hint to the error text the model sees. The hint only fires on the failure, so no prompt bloat.

**File:** `python/llm_browser_worker/worker.py` — wherever the `tool.failed` payload's `error` string is assembled (search for `"tool.failed"` or the path that catches user-code exceptions and stringifies them).

**Implementation:**

```python
_HINT_PATTERNS = [
    (re.compile(r"':contains' is not a valid (CSS )?selector"),
     "':contains' is jQuery, not CSS. Use Array.from(document.querySelectorAll(sel)).filter(el => el.textContent.includes('X'))."),
    (re.compile(r"Identifier '[^']+' has already been declared"),
     "js() shares execution context across calls. Wrap in (()=>{...})() or use var instead of const/let."),
    (re.compile(r"Blocked a frame with origin .+ from accessing a cross-origin frame"),
     "Cross-origin iframe DOM access is impossible. Use iframe_target() to switch CDP context."),
    (re.compile(r"-32602.*No target with given id found"),
     "Target closed or replaced. Call list_tabs() and use a fresh targetId."),
    (re.compile(r"Runtime\.getExecutionContexts.*wasn't found"),
     "Runtime.getExecutionContexts does not exist. Use Target.getTargets or list_tabs()."),
]

def _annotate_error(msg: str) -> str:
    for pat, hint in _HINT_PATTERNS:
        if pat.search(msg):
            return f"{msg}\nHint: {hint}"
    return msg
```

Call `_annotate_error(...)` at the point where `error` is set on the `tool.failed` event. Keep the original message intact; the hint is appended on a new line so any code that parses the error prefix still works.

**Tests:** add to `python/tests/test_worker_package.py`. Five cases — one per pattern — assert the hint suffix is present.

**Acceptance:** run a quick smoke that triggers one of the patterns (e.g. `browser-use-terminal start "test"` then `python … 'js("button:contains(X)")'`) and confirm the hint appears in the `tool.failed` event payload.

---

## 2. Guarded IIFE wrap for `js()`  (#5)

**Goal:** stop the `Identifier 'X' has already been declared` recovery loop without breaking existing JS semantics.

**Risk reminder:** unconditional IIFE wrap breaks (a) bare-expression returns (`js("document.title")`), (b) top-level `await`, (c) cross-call state via `var foo = …`. Mitigate by wrapping **only when needed**.

**File:** `/Users/greg/Documents/browser-use/browser-harness/src/browser_harness/helpers.py` — the `js()` helper that calls `_runtime_evaluate`.

**Approach — two acceptable variants; pick one:**

**(a) Conditional wrap on declaration prefix** (preferred):

```python
_DECL_RE = re.compile(r"^\s*(?:const|let|class)\s", re.MULTILINE)

def js(code, ...):
    if _DECL_RE.search(code) and "return " not in code.splitlines()[-1]:
        # Has block-scoped declarations but no explicit return — IIFE-wrap.
        # Caller wants no return value, just side effects.
        code = f"(()=>{{ {code} }})()"
    return _runtime_evaluate(code)
```

Only wraps when (1) there's a top-level `const`/`let`/`class` *and* (2) the last line is not a `return` statement. If the user wrote explicit `return`, they already know they want IIFE semantics — leave it alone. If there's no top-level declaration, leave it alone (preserves bare-expression returns).

**(b) Catch-and-retry** (safer fallback):

```python
def js(code, ...):
    try:
        return _runtime_evaluate(code)
    except RuntimeError as exc:
        if "already been declared" in str(exc):
            return _runtime_evaluate(f"(()=>{{ {code} }})()")
        raise
```

Slower on the failure case (two CDP round-trips) but zero risk of changing existing behavior. Use this if (a) feels too clever.

**Tests:** add to the browser-harness test suite — three cases:
1. `js("const x = 1; window.foo = x;")` then `js("const x = 2; window.foo = x;")` — both succeed, second call's `window.foo === 2`.
2. `js("document.title")` — returns title (bare-expression return preserved; not wrapped).
3. `js("return 42")` — returns 42 (explicit return preserved).

**Acceptance:** rerun any v14 case where the model previously hit `Identifier 'buttons' has already been declared` (e.g. case 10) and confirm the error no longer recurs.

---

## 3. `page_info` CDP fallback — direct `Target.getTargets` lookup  (#8)

**Goal:** stop `page_info()` returning `{w, h, ...}` without `url`/`title` after `new_tab()` or target replacement.

**File:** `python/llm_browser_worker/worker.py` — `_page_info_cdp_fallback` (line 662).

**Current bug:** the fallback calls `helpers.current_tab()` and only adds `url`/`title` if the daemon's cached tab record has them. Right after `new_tab()` or a target swap, the cache doesn't yet — so `info.get("url")` returns `None`.

**Fix:** query CDP directly when `current_tab()` doesn't supply `url`/`title`:

```python
def _page_info_cdp_fallback(ns, error):
    helpers = ns.get("__browser_harness_helpers__")
    if helpers is None:
        return None
    payload = {}
    # Try cached current_tab first (fast path).
    try:
        tab = helpers.current_tab()
        if isinstance(tab, dict):
            if tab.get("url"):   payload["url"]   = str(tab["url"])
            if tab.get("title"): payload["title"] = str(tab["title"])
    except Exception:
        pass
    # If still missing, query CDP authoritative source.
    if "url" not in payload or "title" not in payload:
        try:
            targets = helpers.cdp("Target.getTargets").get("targetInfos", [])
            active = next(
                (t for t in targets if t.get("attached") and t.get("type") == "page"),
                None,
            )
            if active:
                payload.setdefault("url",   str(active.get("url", "")))
                payload.setdefault("title", str(active.get("title", "")))
        except Exception:
            pass
    # …rest of existing function (viewport metrics via Page.getLayoutMetrics)…
```

If multiple page targets are attached, pick the most-recently-created (last in `targetInfos` is usually newest; or sort by `targetId` if needed). Test by opening a second tab and confirming `page_info()` returns the active tab's URL.

**Tests:** harder to unit-test (needs a live CDP). Add a Python integration test that does `new_tab(URL); info = page_info(); assert info.get("url") == URL`.

**Acceptance:** in any session, after `new_tab("https://example.com")`, calling `page_info()` returns `{"url": "https://example.com", ...}` with a non-None title.

---

## 4. Cloud daemon shutdown on dataset run completion  (#2)

**Goal:** stop billing the Browser Use cloud daemon after a dataset run finishes (or is interrupted), but only if *we* started it.

**Files:**
- `crates/browser-use-cli/src/main.rs` — `dataset_run_provider` (line 1823 ish; the loop over selected cases).
- `crates/browser-use-python-worker/src/lib.rs` — `PythonWorker::drop` (line 267) or a new explicit `shutdown(&mut self)` method.

**Design:** the daemon is owned by the python worker process, not the Rust process. We can't `PATCH /browsers/{id}` from Rust directly without re-implementing auth. Cleanest route: send a python eval through the existing worker IPC just before tearing down.

**Steps:**

1. Add a flag to `AgentRunOptions` (or read directly from `cli_browser_mode()`) so the runner knows whether cloud mode was active.
2. In `dataset_run_provider`, *after* the case loop completes (success, all-failed, or interrupted by signal), if `cli_browser_mode() == "cloud"` and the worker is still alive, send:

   ```python
   try:
       from browser_harness import admin
       admin.stop_remote_daemon()
   except Exception:
       pass
   ```

   via the python tool IPC. The function already exists in browser-harness and does `PATCH /browsers/{id} {"action":"stop"}`.
3. **Gate**: only stop if we own the daemon. Define ownership as "this run started the daemon" — check by reading `LLM_BROWSER_LIVE_URL` from the worker env after the first python call. If it was already set before the run, treat the daemon as user-owned and skip the shutdown call. (Alternatively: a `LLM_BROWSER_OWN_REMOTE_DAEMON=1` env var the runner sets when it spawns the daemon, and checks before stopping.)
4. **Signal handling:** install a `ctrlc` or equivalent handler at the start of `dataset_run_provider` that calls the same shutdown path before exiting non-zero.

**Tests:** can't easily unit-test the cloud API call. Manual smoke:
```bash
LLM_BROWSER_BROWSER_MODE=cloud BU_NAME=tmp-shutdown-smoke \
  uv run browser-use-terminal --state-dir /tmp/but-shutdown-smoke \
  dataset-run-fake real_v14_short --count 1
# After completion, hit Browser Use API and confirm the daemon's status == "stopped".
```

**Acceptance:** after a dataset run completes or is Ctrl-C'd, the cloud daemon does not continue to bill (verifiable via Browser Use dashboard or API).

---

## 5. Provider retry policy + permanent-error stop list  (#3)

**Goal:** retry transient OpenRouter/provider failures at the case level, but never retry errors that are guaranteed to recur.

**Files:** 
- `crates/browser-use-cli/src/main.rs` — `dataset_run_provider` and the per-case attempt loop (look for `max_attempts`).
- `crates/browser-use-providers/src/lib.rs` — the `transient` field on `model.turn.error` events; ensure it's set consistently.

**Two layers:**

### 5a. Case-level retry default

Bump the default `--max-attempts` for `dataset-run-openrouter` / `-codex` / `-anthropic` from 1 to **2**. (Keep `dataset-run-fake` at 1.) Single retry buys recovery from one provider flake per case at modest cost — most cases that fail twice are real failures.

### 5b. Permanent-error stop list

Before retrying a case, inspect the session's final error. If it matches one of these fingerprints, skip the retry and mark `failed (permanent)`:

| Pattern (substring or regex on error text) | Reason |
|---|---|
| `No endpoints found that support image input` | text-only model — recurs on first screenshot |
| `context length exceeded` / `maximum context length` | next attempt's prompt is even bigger |
| `invalid_request_error` with `tool` schema mismatch | tool-call shape bug; deterministic |
| HTTP `401` / `403` | auth |
| `400 Bad Request` with no transient classification *and* attempt 1 already failed mid-session | provider rejected the conversation shape |

Everything else (timeouts, 429, 5xx, JSON parse errors, generic transient 400) → retry once.

**Implementation sketch:**

```rust
fn is_permanent_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    [
        "no endpoints found that support image input",
        "context length exceeded",
        "maximum context length",
        "401",  // be careful — substring match; consider checking status separately
        "403",
    ].iter().any(|needle| lower.contains(needle))
}
```

In the case attempt loop:

```rust
if !ok && attempt < max_attempts && !is_permanent_error(&error) {
    // retry
} else {
    // record final outcome
}
```

### 5c. Pre-flight vision capability probe (optional, nice-to-have)

To catch the text-only model case (DeepSeek V4 Pro) *before* spending any browser time, add a `--probe-vision` flag (default true) that sends one tiny request with a 1×1 image at startup. Cache the result by `(provider, model)` in the auth/config store. On `"No endpoints found that support image input"`, abort the run with a clear error before booking any browser.

Skip this in v1 if it complicates the PR — the stop-list (5b) already handles it by failing fast.

**Tests:** unit-test `is_permanent_error` with the table above. Integration test in `dataset_run_provider`: simulate a fake provider that returns a transient error on attempt 1 and success on attempt 2, assert the case passes with `attempt_number=2` in the manifest.

**Acceptance:** rerun the failed v14 cases with `--max-attempts 2`. Case 8 (response-body timeout) should retry and either pass or fail with a clean second error. Case 4 (hard 400) should mark permanent and skip retry.

---

## 6. Cost accounting — make it actually work for everything  (#7)

**Goal:** every provider reports accurate `cost_usd` per call. OpenRouter dated/private models stop showing `$0.00`.

**File:** `crates/browser-use-providers/src/lib.rs`.

### 6a. OpenRouter native cost (highest leverage)

OpenRouter returns the exact USD charge if the request body includes `"usage": {"include": true}`. The response then carries `usage.cost` (and `usage.cost_details.upstream_inference_cost`). This is **authoritative** — no pricing table needed for any OpenRouter model, including dated ones we don't have entries for.

**Steps:**

1. In `OpenAICompatibleChatProvider::start_turn` (line 245), inject the field into `body`:

   ```rust
   body["usage"] = json!({"include": true});
   ```

   Always — non-OpenRouter providers (Fireworks/Together/Groq) ignore unknown fields. Tested empirically; if any provider rejects, gate by `self.base_url.contains("openrouter.ai")`.

2. In `parse_chat_usage` (line 1719), also pull `usage.cost`:

   ```rust
   let cost_usd = usage.get("cost").and_then(Value::as_f64);
   let mut model_usage = parse_usage_common(...);
   if let Some(c) = cost_usd { model_usage.cost_usd = Some(c); }
   ```

3. In `apply_pricing` (line 1877), check if `cost_usd.is_some()` already and short-circuit:

   ```rust
   fn apply_pricing(usage: &mut ModelUsage, model: &str) {
       if usage.cost_usd.is_some() {
           return;  // provider supplied authoritative cost
       }
       // …existing table lookup…
   }
   ```

### 6b. Extend prefix loop to include `openrouter/`

In `find_model_pricing_data` (line 1986), add `"openrouter/"` to the prefix loop. One line. Helps non-dated OpenRouter slugs that LiteLLM does index as `openrouter/x`.

### 6c. Strip dated suffixes before LiteLLM lookup

Models like `qwen/qwen3.6-plus-04-02` and `deepseek/deepseek-v4-pro-20260415` aren't in LiteLLM's table by their dated names — but the un-dated parent often is. Add a stripping pass in `find_model_pricing_data`:

```rust
// Try stripping a trailing -MM-DD or -YYYYMMDD before any other lookup
fn strip_date_suffix(model: &str) -> Option<String> {
    let re = regex::Regex::new(r"-\d{2}-\d{2}$|-\d{8}$").ok()?;
    let stripped = re.replace(model, "");
    if stripped != model { Some(stripped.into_owned()) } else { None }
}
```

Apply after the exact-match attempt fails: try the stripped form, then the prefix loop on the stripped form.

### 6d. Codex streaming usage

In `parse_usage` (line 1820 — used by the Codex stream path at line 905), also accept `usage.cost_usd` and `usage.cost`. Codex's responses sometimes include these on the final `response.completed` event. Same short-circuit semantics in `apply_pricing`.

### 6e. Anthropic — verify cache pricing works

The parsing at line 1820 already reads `cache_read_input_tokens` and `cache_creation_input_tokens` from the Anthropic `usage` field. The pricing applies via `apply_pricing`. **Verify** by running a real Anthropic call with caching enabled and checking `usage.input_cached_cost_usd > 0`. If broken, the culprit is a key mismatch in LiteLLM — investigate `pricing_data.get("claude-sonnet-4.6")` vs `"anthropic/claude-sonnet-4.6"`. No code change expected here; ship a regression test instead.

### 6f. Don't lie about `$0.00` when lookup failed

In the dataset manifest aggregation (search for where per-session `cost_usd` is summed into `summary.usage.cost_usd`), propagate `None` if any session had no cost. Surface a `summary.usage.cost_unknown_count` so missed lookups are visible instead of silently summing to `0.0`.

**Tests:**

- Unit-test `strip_date_suffix`: `"qwen/qwen3.6-plus-04-02"` → `"qwen/qwen3.6-plus"`; `"gpt-5.5"` → `None`.
- Unit-test `find_model_pricing_data` with mock LiteLLM data: confirms `"openrouter/"` prefix is tried and dated-suffix stripping works.
- Mock-server test for OpenRouter native cost: provider returns `{"usage": {"cost": 0.0123, "prompt_tokens": 100, ...}}`, assert `ModelUsage.cost_usd == Some(0.0123)` and `apply_pricing` is a no-op.
- Anthropic cache regression test: feed a fixture response with `cache_read_input_tokens > 0` and a known cached pricing entry, assert `input_cached_cost_usd` is non-zero.

**Acceptance:** rerun a single OpenRouter case (`dataset-run-openrouter real_v14_short --count 1 --model qwen/qwen3.6-plus-04-02`) and confirm the manifest's `summary.usage.cost_usd > 0`. Spot-check the per-session `cost_usd` against OpenRouter's dashboard for the same request.

---

## Suggested PR order

Lowest risk → highest:

1. **#1** error hints (worker.py, ~40 LOC, zero side effects)
2. **#3** `page_info` fallback (worker.py, ~15 LOC, fixes a real bug)
3. **#4** daemon cleanup (Rust + worker, ~50 LOC, gated by cloud mode)
4. **#2** IIFE wrap — conditional variant (helpers.py, ~15 LOC)
5. **#5** retry + stop list (Rust, ~80 LOC)
6. **#6** cost — split into two PRs:
   - 6a (OpenRouter native cost) and 6f (don't lie about $0) — small, immediate
   - 6b/6c/6d (table coverage + dated stripping) — follow-up
   - 6e (Anthropic verification) — investigate first, code only if broken

Each item is independent — merge in any order. None of them depend on the browser-event streaming work in `plan-browser-events-to-model.md`.

---

## Out of scope for this plan

- Browser event streaming (#10) — see `plan-browser-events-to-model.md`.
- Bumping `--max-turns` default — discuss separately; not a bug.
- Adding a min-result-chars gate for `done` — discuss separately; the false-positive case 6 is real but the fix involves design choices around partial credit.
- New cloud-browser providers, new model providers, new dataset shapes.
