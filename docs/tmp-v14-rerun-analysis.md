# TMP: real_v14_short Failed-Case Rerun Analysis

Date: 2026-05-11

Scope: rerun previously failed or behaviorally failed `real_v14_short` cases with the OpenAI provider and Browser Use cloud browser after the generalizable harness fixes.

## Runs

| Task | Session | Local status | Main outcome |
| --- | --- | --- | --- |
| 4 | `4ae12e45c94b` | failed | OpenAI response body JSON parse failure after first Python screenshot/tool result. |
| 5 | `54f7fbf4d533` | failed | OpenAI cybersecurity-risk refusal after 15 invocations. |
| 6 | `7d180ae5c31b` | failed | OpenAI request timeout after retries were exhausted. |
| 10 | `55fe679ae3f4` | failed | OpenAI cybersecurity-risk refusal after 7 invocations. |
| 16 | `ac9ad79a0ecb` | passed | McDonald's menu extraction completed; retries recovered from transient timeouts. |

## What Improved

- The previous Codex protocol failure, `No tool call found for function call output`, did not reproduce in these OpenAI reruns.
- Provider retry events are now visible and useful. Task 16 recovered after three OpenAI request timeouts and still completed.
- Event replay kept image-bearing Python outputs paired with tool calls through long histories. Task 16 reached 23 model invocations, 22 Python calls, and 11 images without a tool-output protocol failure.
- Full error strings now preserve enough provider context to classify failures from manifests and event streams.

## What Still Broke

### 1. Empty Or Invalid Provider Response Body

Task 4 failed on:

```text
parse OpenAI Responses JSON: error decoding response body: expected value at line 1 column 1
```

This happened after the first browser screenshot and tool output. The failure is transport-shaped: the model request reached OpenAI, but the response body was empty or non-JSON. The current transient classifier did not retry it.

Generalizable fix:

- Treat `parse OpenAI Responses JSON` with `expected value at line 1 column 1` as transient.
- Ideally include HTTP status, content type, and a short redacted body preview in the error if available.
- Add a regression test where the provider returns an empty body once and then succeeds.

### 2. Provider Policy Refusals

Tasks 5 and 10 failed with:

```text
This content was flagged for possible cybersecurity risk.
```

These are not harness failures and should not be retried blindly. The refusals appeared after useful browser progress, not necessarily on the initial task prompt.

Generalizable fix:

- Keep these classified as terminal provider failures.
- Record the turn index and prompt/message fingerprint so we can locate what changed right before the refusal.
- For eval workflows, report these separately from harness failures.

### 3. Request Timeout Budget Was Sometimes Not Enough

Task 6 failed on repeated OpenAI request timeouts. The retry wrapper worked, but exhausted the OpenAI retry budget.

Generalizable fix:

- Keep the per-turn retry wrapper.
- Consider a higher retry budget for long browser runs via `LLM_BROWSER_PROVIDER_MAX_RETRIES`.
- Consider increasing provider HTTP timeout or adding a longer timeout after the first retry.
- Add dataset-level retry only for transient provider failures, so a whole task can restart after per-turn retries are exhausted.

### 4. Cost Calculation Still Has No Pricing For `gpt-5.5`

Usage tokens are present, but `cost_usd` is null in `model.usage` events and summarizes to `0.0`.

Generalizable fix:

- Add an explicit `gpt-5.5` pricing entry once the intended pricing is known.
- Until pricing is known, report token totals and clearly mark cost as unavailable instead of `0.0`.

## Current Read

The biggest remaining breakage in this rerun was not browser control. The browser/Python harness survived long image-bearing histories. The next fixes should focus on provider robustness and eval reporting:

- retry empty/invalid transient OpenAI response bodies
- distinguish provider policy failures from harness failures in analysis
- make timeout retry budgets configurable per run
- fix or explicitly mark unavailable cost accounting for `gpt-5.5`
