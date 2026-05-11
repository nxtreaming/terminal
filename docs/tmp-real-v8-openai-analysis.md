# TMP: real_v8 OpenAI Remote-Browser Eval Analysis

Date: 2026-05-11

Scope: `real_v8` dataset, 100 tasks, OpenAI provider (`gpt-5.5`), Browser Use cloud browser (`LLM_BROWSER_BROWSER_MODE=cloud`), one isolated local process per task.

Primary run directory:

```text
/tmp/but-real-v8-openai-20260511-090201
```

Task 66 initially looked stale while the shell fanout was still waiting, so it was rerun separately:

```text
/tmp/but-real-v8-openai-20260511-task66-rerun
```

The primary task-66 process later completed and wrote its manifest. The summary below uses the primary run for all 100 tasks. The task-66 rerun is excluded from aggregate counts; it also failed with the same OpenAI cyber-policy class after 24 invocations.

## Caveat

The local dataset runner marks a task as passed when the session calls `done` with a result. This is not a judge. Some local passes may still be factually wrong or incomplete. Two examples already look suspicious from result size alone: task 13 returned 1 character, and task 45 returned 5 characters.

## Headline Results


| Class                                | Count | Task IDs                                                                                                                                                                                        |
| ------------------------------------ | ----- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Local pass                           | 31    | 2, 11, 13, 14, 16, 18, 20, 35, 37, 42, 45, 48, 49, 51, 53, 56, 60, 61, 64, 69, 70, 71, 77, 78, 83, 84, 86, 90, 91, 97, 98                                                                       |
| OpenAI cyber-policy failure          | 49    | 3, 4, 5, 9, 19, 21, 22, 23, 26, 27, 28, 29, 30, 32, 33, 34, 36, 38, 39, 40, 41, 43, 44, 46, 50, 54, 55, 57, 58, 59, 62, 63, 65, 66, 67, 68, 72, 73, 76, 79, 80, 82, 85, 87, 89, 93, 95, 96, 100 |
| OpenAI request timeout after retries | 18    | 6, 8, 10, 12, 15, 17, 24, 25, 31, 47, 52, 74, 75, 81, 88, 92, 94, 99                                                                                                                            |
| Empty / invalid OpenAI JSON body     | 2     | 1, 7                                                                                                                                                                                            |


Aggregate usage from manifests:


| Metric                | Value      |
| --------------------- | ---------- |
| Model invocations     | 1,341      |
| Model turn requests   | 1,410      |
| Tool calls            | 1,336      |
| Image outputs         | 352        |
| Provider retry events | 162        |
| Provider error events | 231        |
| Input tokens          | 31,423,477 |
| Output tokens         | 428,506    |
| Total tokens          | 31,851,983 |


Cost fields in the manifests are wrong: usage events store cost fields as null and summaries show `0.0`, even though token counts are present. Using OpenAI's GPT-5.5 standard pricing on 2026-05-11 (`$5.00 / 1M` uncached input, `$0.50 / 1M` cached input, `$30.00 / 1M` output), the estimated model API cost is:


| Component      | Tokens     | Rate        | Cost   |
| -------------- | ---------- | ----------- | ------ |
| Uncached input | 8,193,013  | $5.00 / 1M  | $40.97 |
| Cached input   | 23,230,464 | $0.50 / 1M  | $11.62 |
| Output         | 428,506    | $30.00 / 1M | $12.86 |
| Total          | 31,851,983 | mixed       | $65.44 |


No single request exceeded 272K input tokens in the recorded `model.usage` events, so the GPT-5.5 long-context surcharge should not apply to this run. Browser Use cloud browser costs, if any, are not included here.

Cost by final local class:


| Class                        | Estimated cost |
| ---------------------------- | -------------- |
| Local pass                   | $20.08         |
| OpenAI cyber-policy failure  | $24.73         |
| OpenAI timeout failure       | $18.87         |
| Empty / invalid JSON failure | $1.76          |


## What Went Better

The browser/tool replay path held up under much heavier stress than the previous `real_v14_short` run:

- 1,336 tool calls and 352 image outputs completed without the old unmatched tool-output protocol error.
- Several successful runs had long histories:
  - task 78: 29 invocations
  - task 16: 28 invocations
  - task 98: 27 invocations and 12 images
  - task 42: 24 invocations
  - task 60: 22 invocations
- Several failed runs also reached long image-bearing histories before provider failure:
  - task 66: 63 turns, 62 tools, 3 images, then policy block
  - task 47: 28 turns, 27 tools, 24 images, then timeout
  - task 17: 37 turns, 36 tools, 17 images, then timeout
  - task 27: 31 turns, 30 tools, 11 images, then policy block

This suggests the event-history replay and image/tool pairing fixes are directionally correct. The dominant breakage is now provider-side.

## Where The Model Breaks Now

### 1. OpenAI Cyber-Policy Blocks Dominate

49 of 100 tasks ended with:

```text
This content was flagged for possible cybersecurity risk.
```

Only 5 were immediate prompt blocks with zero model invocations:

```text
4, 23, 34, 59, 65
```

The rest happened after useful browser/tool work. The largest late policy failures were:


| Task | Invocations | Turns | Tools | Images |
| ---- | ----------- | ----- | ----- | ------ |
| 66   | 62          | 63    | 62    | 3      |
| 57   | 33          | 34    | 33    | 8      |
| 27   | 30          | 31    | 30    | 11     |
| 9    | 26          | 27    | 26    | 6      |
| 33   | 25          | 26    | 25    | 5      |
| 38   | 23          | 24    | 23    | 8      |
| 79   | 22          | 23    | 22    | 5      |
| 50   | 21          | 22    | 21    | 8      |


This is a routing/provider problem more than a browser-control problem. The model gets blocked by some combination of task text, page text, tool output, names/addresses, URLs, extracted database content, or accumulated context. We currently only store metadata/fingerprints unless `LLM_BROWSER_RECORD_MODEL_IO=true`, so exact trigger attribution needs targeted reruns with full I/O capture.

Generalizable fixes:

- Report provider policy failures separately from harness failures.
- Record turn index, message/tool fingerprints, counts, and last tool name in dataset summaries.
- For a small failing subset, rerun with `LLM_BROWSER_RECORD_MODEL_IO=true` to identify the actual policy-triggering message content.
- Consider provider fallback/routing for evals where OpenAI policy blocks benign browsing/data extraction tasks.

### 2. OpenAI Request Timeouts Are Common

18 tasks failed after exhausting per-turn retries:

```text
6, 8, 10, 12, 15, 17, 24, 25, 31, 47, 52, 74, 75, 81, 88, 92, 94, 99
```

The retry wrapper is working: the run recorded 162 retry events. But a 100-way fanout creates enough pressure that some turns still exhaust the OpenAI retry budget.

Important detail: task 66 also hit repeated timeouts in the isolated rerun, so this is not only caused by 100-way parallelism. Long contexts plus image/tool-heavy histories seem to increase timeout frequency.

Generalizable fixes:

- Increase OpenAI retry budget for eval runs with `LLM_BROWSER_PROVIDER_MAX_RETRIES`.
- Use longer provider HTTP timeouts or progressive timeout growth after retries.
- Add dataset-level retry for transient provider failures, so a task can restart after one per-turn retry exhaustion.
- Add built-in parallel dataset runner with controlled concurrency and status tracking instead of shell-level process fanout.

### 3. Empty / Invalid JSON Response Bodies Are Still Terminal

Tasks 1 and 7 failed with:

```text
parse OpenAI Responses JSON: error decoding response body: expected value at line 1 column 1
```

Task 7 failed before a model invocation. Task 1 failed after 32 invocations, 32 tool calls, 2 images, and 4 retry events.

Generalizable fix:

- Classify empty-body / column-1 JSON decode errors as transient provider failures.
- Retry them through the same reconnect wrapper.
- Include HTTP status, content type, and a short redacted body preview when JSON parsing fails.

### 4. Local Pass Is Not A Quality Signal

31 tasks ended locally as `done`, but the local runner does not validate correctness. Some output sizes are clearly suspicious.

Examples:


| Task | Invocations | Final chars |
| ---- | ----------- | ----------- |
| 13   | 12          | 1           |
| 45   | 22          | 5           |
| 86   | 9           | 137         |
| 97   | 20          | 277         |


Generalizable fixes:

- Run an LLM judge or schema/task-specific validator over every final answer.
- Mark local status as `completed`, not `passed`, unless judged.
- Track final result size and structured-output parse status in the manifest summary.

### 5. Shell Fanout Made Long-Running State Hard To Read

Task 66 initially looked stale:

- SQLite session status: `running`
- no `status/task-66.exit` file from the shell wrapper
- no final manifest session entry

It later completed in the primary run with OpenAI cyber-policy after 62 invocations. The isolated rerun also failed with policy after 24 invocations.

This is mostly an observability/supervision problem with the ad hoc shell fanout. The product needs first-class supervision for this workflow so long-running tasks cannot look orphaned while they are still finishing.

Generalizable fixes:

- Add a repo-owned parallel dataset runner instead of shelling out 100 independent processes.
- Record child process PID, heartbeat, start/end timestamps, and final exit status.
- Show task-level progress in one aggregate manifest while tasks are running.
- Detect truly stale `running` sessions whose process is gone.
- Finalize truly stale sessions as `failed` with a clear `runner_lost_process` error.

## Current Interpretation

The core browser harness is no longer the primary failure. Under `real_v8` stress, the model breaks mostly at the provider boundary:

- OpenAI policy blocks benign browser/data tasks.
- OpenAI request timeouts become frequent with high parallelism and long contexts.
- Empty JSON provider responses need retry classification.
- Result quality still needs judging; local `done` is too weak.

The best next engineering fixes are provider/eval infrastructure:

1. Retry empty JSON provider bodies.
2. Add dataset-level retry for transient provider failures.
3. Add a real parallel dataset runner with process supervision.
4. Improve eval reporting: provider-policy vs timeout vs harness vs judged task failure.
5. Rerun a small policy-failure subset with full model I/O capture to identify policy triggers.
6. Add `gpt-5.5` pricing or explicitly show cost as unavailable.

