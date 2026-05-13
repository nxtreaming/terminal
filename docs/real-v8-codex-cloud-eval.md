# real_v8 Codex Cloud Eval Runbook

This runbook describes how to run the `real_v8` benchmark with Codex login, Browser Use cloud browsers, the built-in fixed-concurrency dataset runner, and batched manual judging by subagents.

The goal is a practical stress eval, not a polished benchmark harness. Treat the local runner's `ok: true` as "the agent produced a final answer", not as a judged task success.

## When To Use This

Use this workflow when you want to:

- Run all 100 `real_v8` tasks quickly.
- Use the Codex provider path (`dataset-run-codex`) rather than OpenAI API-key auth.
- Use remote Browser Use cloud browsers (`--browser-mode cloud`).
- Keep 25 tasks running concurrently until the selected dataset is drained.
- Give every task attempt an isolated workspace and artifact directory.
- Judge completed task quality manually with subagents, in batches.

For stable benchmark scoring, rerun provider/timeouts at lower concurrency before counting them against the agent. A 25-way run is still a stress run, but it is less likely to create provider timeouts than higher fanout.

## Repo Assumptions

Run commands from the repository root:

```bash
cd /Users/greg/Documents/browser-use/experiments/llm-browser-rust-rewrite
```

The relevant CLI commands are:

```bash
./target/debug/browser-use-terminal auth import-codex --from ~/.codex/auth.json
./target/debug/browser-use-terminal auth status
./target/debug/browser-use-terminal dataset-run-codex real_v8 --task-id 1
./target/debug/browser-use-terminal sessions export <SESSION_ID> <OUTPUT_DIR>
```

## Preflight

Build the CLI:

```bash
cargo build -p browser-use-cli
```

Check the dataset is available:

```bash
./target/debug/browser-use-terminal dataset-list
./target/debug/browser-use-terminal dataset-sample real_v8 --count 3
```

Check Codex login. Prefer importing the active Codex login from `~/.codex/auth.json`; this makes each isolated task state self-contained.

```bash
RUN_ID="real-v8-codex-cloud-$(date +%Y%m%d-%H%M%S)"
OUT="/tmp/$RUN_ID"
mkdir -p "$OUT"

./target/debug/browser-use-terminal \
  --state-dir "$OUT/auth-check" \
  auth import-codex --from ~/.codex/auth.json

./target/debug/browser-use-terminal \
  --state-dir "$OUT/auth-check" \
  auth status
```

If import fails, either fix the Codex CLI login first or use explicit Codex env vars:

```bash
export LLM_BROWSER_CODEX_ACCESS_TOKEN="..."
export LLM_BROWSER_CODEX_ACCOUNT_ID="..."
```

## Run All Tasks With 25-Way Fixed Concurrency

The dataset runner owns scheduling. It keeps up to 25 task workers active, starts the next task as soon as one finishes, writes one aggregate manifest, and gives every task attempt its own filesystem tree under `dataset-run-files/<run-id>/`.

Each worker is forced into Browser Use cloud mode. It also disables local CDP/browser environment overrides so a stale local Chrome cannot be reused by accident.

```bash
RUN_ID="${RUN_ID:-real-v8-codex-cloud-$(date +%Y%m%d-%H%M%S)}"
OUT="${OUT:-/tmp/$RUN_ID}"
STATE_DIR="${STATE_DIR:-$OUT/state}"
mkdir -p "$OUT/logs" "$STATE_DIR"

./target/debug/browser-use-terminal \
  --state-dir "$STATE_DIR" \
  auth import-codex --from ~/.codex/auth.json

LLM_BROWSER_PROVIDER_MAX_RETRIES=5 \
./target/debug/browser-use-terminal \
  --state-dir "$STATE_DIR" \
  dataset-run-codex real_v8 \
  --all \
  --model gpt-5.5 \
  --max-turns 80 \
  --python-timeout-seconds 180 \
  --max-attempts 2 \
  --concurrency 25 \
  --browser-mode cloud \
  --run-id "$RUN_ID" \
  2>&1 | tee "$OUT/logs/dataset-run.log"

MANIFEST="$STATE_DIR/dataset-runs/$RUN_ID.json"
TASK_FILES="$STATE_DIR/dataset-run-files/$RUN_ID"
```

Notes:

- Use `--concurrency 25` for the default stress run.
- Use `--concurrency 10` or `--concurrency 15` for a cleaner provider-quality score.
- Set `LLM_BROWSER_RECORD_MODEL_IO=true` only for targeted reruns. Full model I/O capture across 100 tasks can produce large artifacts and expose sensitive page content.
- If the machine, provider, or cloud browser pool starts thrashing, stop the run and resume failed tasks at lower concurrency.
- Agents should write large outputs to `/home/user/outputs`, which maps to each task attempt's real `outputs/` directory under `$TASK_FILES`.

## Build A Run Summary

Create a compact summary from the aggregate manifest:

```bash
jq '
  . as $manifest
  | .sessions
  | map({
      task_id,
      local_ok: (.ok // false),
      provider: (.provider // $manifest.provider),
      model: (.model // $manifest.model),
      browser: $manifest.browser,
      session_id: (.session.id // null),
      session_status: (.session.status // null),
      cwd: (.session.cwd // null),
      artifact_root: (.session.artifact_root // null),
      final_result_chars: (.final_result_chars // 0),
      error_type: (.error_type // null),
      error: (.error // null),
      usage: (.usage // null)
    })
  | sort_by((.task_id | tonumber))
' "$MANIFEST" > "$OUT/run-summary.json"
```

Quick counts:

```bash
jq 'group_by(.local_ok) | map({local_ok: .[0].local_ok, count: length})' "$OUT/run-summary.json"
jq -r '.[] | select(.local_ok == false) | [.task_id, .error_type, (.error // "")] | @tsv' "$OUT/run-summary.json"
jq -r '.[] | select(.local_ok == true and .final_result_chars < 50) | [.task_id, .final_result_chars] | @tsv' "$OUT/run-summary.json"
```

Find missing manifests:

```bash
comm -23 \
  <(jq -r '.selection[].task_id' "$MANIFEST" | sort -n) \
  <(jq -r '.[].task_id' "$OUT/run-summary.json" | sort -n)
```

## Export Trace Bundles

Export every available session. These exports are the evidence packets for manual judging.

```bash
mkdir -p "$OUT/exports"

for task in $(jq -r '.[].task_id' "$OUT/run-summary.json"); do
  export_dir="$OUT/exports/task-$task"
  session_id="$(jq -r --arg task "$task" '.[] | select(.task_id == $task) | .session_id // empty' "$OUT/run-summary.json")"
  [ -n "$session_id" ] || continue

  mkdir -p "$export_dir"

  ./target/debug/browser-use-terminal \
    --state-dir "$STATE_DIR" \
    sessions export "$session_id" "$export_dir" \
    > "$OUT/logs/task-$task.export.out" \
    2> "$OUT/logs/task-$task.export.err"

  ./target/debug/browser-use-terminal \
    --state-dir "$STATE_DIR" \
    events "$session_id" \
    > "$export_dir/events.txt" \
    2> "$OUT/logs/task-$task.events.err"
done
```

## Create Judge Packets

The first judge pass should use compact packets. Do not send every raw trace and screenshot to a batched judge unless needed; large batches reduce attention and increase cost.

```bash
mkdir -p "$OUT/judge-packets"

for task in $(jq -r '.[].task_id' "$OUT/run-summary.json"); do
  packet="$OUT/judge-packets/task-$task.md"

  {
    echo "# Task $task"
    echo
    echo "## Original Task"
    jq -r --arg task "$task" '.selection[] | select(.task_id == $task) | .confirmed_task' "$MANIFEST"
    echo
    echo "## Local Runner Result"
    jq --arg task "$task" '.[] | select(.task_id == $task) | {local_ok, error_type, error, session_id, session_status, cwd, artifact_root, final_result_chars}' "$OUT/run-summary.json"
    echo
    echo "## Final Answer"
    jq -r --arg task "$task" '.sessions[] | select(.task_id == $task) | .final_result // ""' "$MANIFEST"
    echo
    echo "## Task Files"
    jq -r --arg task "$task" '.[] | select(.task_id == $task) | "cwd: \(.cwd // "")\nartifact_root: \(.artifact_root // "")"' "$OUT/run-summary.json"
    echo
    echo "## Exported Files"
    if [ -d "$OUT/exports/task-$task" ]; then
      find "$OUT/exports/task-$task" -maxdepth 3 -type f | sort
    else
      echo "No export directory found."
    fi
  } > "$packet"
done
```

## Batched Subagent Judging

Default judge batch size: 5 tasks per subagent.

Use 10 tasks per judge only when packets are small and mostly provider failures. Use one task per judge for ambiguous local completions, very short answers, complex extraction, or cases where screenshot/artifact inspection is required.

Recommended first-pass grouping:

- Group local completions together.
- Group provider failures together.
- Group short/suspicious completions together.
- Do not mix huge traces with tiny traces in the same judge batch.

The judge should produce JSONL, one line per task:

```json
{"task_id":"13","verdict":"failure","confidence":"high","failure_class":"bad_final_answer","reason":"The final answer is one character and does not satisfy the requested extraction or source URL requirements."}
{"task_id":"42","verdict":"success","confidence":"medium","failure_class":null,"reason":"The answer includes the requested extracted fields and source URL; no obvious contradiction was found in the provided packet."}
```

Allowed verdicts:

- `success`
- `partial`
- `failure`
- `not_judgable`

Recommended failure classes:

- `provider_policy_block`
- `provider_timeout`
- `provider_empty_response`
- `browser_harness_failure`
- `anti_bot_or_login_block`
- `bad_final_answer`
- `missing_required_source_url`
- `missing_required_fields`
- `invalid_structured_output`
- `insufficient_evidence`
- `stopped_too_early`

Judge prompt template:

```text
You are judging browser-agent benchmark tasks.

For each task packet, compare the original task requirements with the final answer and available evidence.

Do not treat local_ok=true as success. local_ok only means the agent produced a final answer.
Mark success only if the final answer satisfies the task requirements.
If the task asks for structured JSON, required attributes, source URLs, or no-login behavior, check those explicitly.
If evidence is too thin to verify a local completion, use not_judgable or partial with a clear reason.
If the run failed before a final answer because of provider policy, timeout, or invalid provider response, classify that directly.

Return JSONL only, one object per task:
{"task_id":"...","verdict":"success|partial|failure|not_judgable","confidence":"high|medium|low","failure_class":"... or null","reason":"short concrete explanation"}
```

## Suggested Judge Batching

Make a task list for first-pass judging:

```bash
jq -r '.[] | [.task_id, .local_ok, .final_result_chars, (.error_type // ""), ((.error // "") | gsub("\n"; " ") | .[0:120])] | @tsv' \
  "$OUT/run-summary.json" \
  > "$OUT/judge-index.tsv"
```

For most runs, start with local completions:

```bash
jq -r '.[] | select(.local_ok == true) | .task_id' "$OUT/run-summary.json" \
  > "$OUT/local-completions.txt"
```

Then assign 5 tasks per judge:

```bash
awk '{batch=int((NR-1)/5)+1; print > "'"$OUT"'/judge-batch-" batch ".txt"}' "$OUT/local-completions.txt"
```

For each `judge-batch-N.txt`, give the judge subagent:

- The judge prompt template.
- The listed `judge-packets/task-<id>.md` files.
- Relevant exported screenshots/artifacts only when needed.

Escalate these to single-task judges:

```bash
jq -r '.[] | select(.local_ok == true and .final_result_chars < 200) | .task_id' "$OUT/run-summary.json" \
  > "$OUT/escalate-short-local-completions.txt"
```

Also escalate any batch verdict with `confidence: low`, `verdict: partial`, or `verdict: not_judgable`.

## Merge Judge Results

Save all judge JSONL output under:

```text
$OUT/judgments/
```

Merge:

```bash
mkdir -p "$OUT/judgments"
cat "$OUT"/judgments/*.jsonl > "$OUT/judgments.jsonl"
```

Summarize:

```bash
jq -s '
  group_by(.verdict)
  | map({verdict: .[0].verdict, count: length, task_ids: map(.task_id)})
' "$OUT/judgments.jsonl"

jq -s '
  group_by(.failure_class)
  | map({failure_class: .[0].failure_class, count: length, task_ids: map(.task_id)})
' "$OUT/judgments.jsonl"
```

Join local runner status with judgments:

```bash
jq -s '
  .[0] as $summary
  | .[1] as $judgments
  | ($judgments | map({key: .task_id, value: .}) | from_entries) as $judgment_by_task
  | $summary
  | map(. as $row
      | ($judgment_by_task[$row.task_id] // {}) as $judge
      | $row + {judged_verdict: $judge.verdict, judged_confidence: $judge.confidence, judged_failure_class: $judge.failure_class, judged_reason: $judge.reason}
    )
' "$OUT/run-summary.json" <(jq -s '.' "$OUT/judgments.jsonl") \
  > "$OUT/final-scored-results.json"
```

## Failure Mode Analysis Report

After judging, produce one readable Markdown report that answers four questions:

1. What is the current dataset score?
2. What failure modes happened?
3. Why did they happen, based on traces and the current codebase?
4. Which fixes should be prioritized by impact and effort?

Write the report to:

```text
$OUT/real-v8-eval-analysis.md
```

### Scorecard

Use judged results as the score of record. Local runner status is supporting evidence only.

Useful score queries:

```bash
jq '
  group_by(.judged_verdict // "unjudged")
  | map({verdict: (.[0].judged_verdict // "unjudged"), count: length, task_ids: map(.task_id)})
' "$OUT/final-scored-results.json"

jq '
  {
    total: length,
    judged_success: (map(select(.judged_verdict == "success")) | length),
    judged_partial: (map(select(.judged_verdict == "partial")) | length),
    judged_failure: (map(select(.judged_verdict == "failure")) | length),
    not_judgable: (map(select(.judged_verdict == "not_judgable")) | length),
    unjudged: (map(select(.judged_verdict == null)) | length),
    local_ok: (map(select(.local_ok == true)) | length)
  }
' "$OUT/final-scored-results.json"
```

Report both strict and generous scores:

- Strict score: `success / 100`.
- Generous score: `(success + partial) / 100`.
- Local completion rate: `local_ok / 100`, clearly labeled as not judged quality.

### Failure Mode Inventory

Aggregate failure classes:

```bash
jq '
  map(select(.judged_verdict != "success"))
  | group_by(.judged_failure_class // .error_type // "unknown")
  | map({
      failure_class: (.[0].judged_failure_class // .[0].error_type // "unknown"),
      count: length,
      task_ids: map(.task_id),
      sample_reasons: map(.judged_reason // .error // "") | unique | .[0:5]
    })
  | sort_by(-.count)
' "$OUT/final-scored-results.json" \
  > "$OUT/failure-modes.json"
```

Add a compact visual table to the report:

```bash
jq -r '
  (["| Failure mode | Count | Tasks |", "| --- | ---: | --- |"][]),
  (.[] | "| \(.failure_class) | \(.count) | \(.task_ids | join(", ")) |")
' "$OUT/failure-modes.json" \
  > "$OUT/failure-modes-table.md"
```

For a simple bar chart section:

```bash
jq -r '
  .[]
  | "\(.failure_class)\n" + ("#" * .count) + " \(.count)\n"
' "$OUT/failure-modes.json" \
  > "$OUT/failure-modes-bars.txt"
```

### Root Cause Analysis

For each major failure mode, inspect representative evidence:

- The task prompt in `judge-packets/task-<id>.md`.
- The final answer and local error from `run-summary.json`.
- Exported events in `exports/task-<id>/events.txt`.
- Screenshots, downloaded files, and other artifacts in `exports/task-<id>/`.
- Per-task stdout/stderr under `logs/`.

Map each mode to a likely codebase or setup cause:

| Failure area | Evidence to inspect | Likely code to inspect |
| --- | --- | --- |
| Provider policy block | Provider error text, turn count, task topic, last tool events | `crates/browser-use-providers`, provider retry/classification in `crates/browser-use-cli/src/main.rs` |
| Provider timeout | Retry events, stderr, long histories, image-heavy traces | `crates/browser-use-providers`, timeout/retry settings, dataset runner retry logic |
| Empty provider response | JSON parse errors, HTTP metadata if present | provider response parsing and transient error classification |
| Bad final answer | Original task vs final answer, missing fields/source URL/schema | dataset prompt, model instructions, final-answer handling |
| Browser harness failure | Python worker events, CDP errors, screenshot gaps | `crates/browser-use-core`, `crates/browser-use-python-worker`, browser harness prompts |
| Anti-bot/login block | Screenshots, page text, task instruction handling | browser control strategy, prompts, cloud browser profile support |
| Stopped too early | Low turn count, superficial final answer, missing extraction | stopping criteria, prompt pressure to verify before final |

Do not overfit from a single task. For each failure mode, use at least 2-3 representative task IDs when possible.

### Fix Ranking

Rank fixes by:

- Impact: expected number of affected tasks.
- Confidence: how clearly the evidence points to this cause.
- Effort: implementation and validation cost.
- Risk: chance of introducing regressions.

Use this table shape:

| Priority | Fix | Failure modes addressed | Expected impact | Effort | Confidence | Owner area | Validation |
| ---: | --- | --- | ---: | --- | --- | --- | --- |
| P0 | Retry empty provider bodies as transient | `provider_empty_response` | High | Low | High | provider/CLI | rerun affected tasks |
| P0 | Classify provider policy/timeouts separately from task failures | `provider_policy_block`, `provider_timeout` | High | Low | High | eval reporting | summary reflects not-judgable/provider |
| P1 | Add supervised parallel dataset runner | lost process state, unclear fanout status | Medium | Medium | High | CLI eval runner | 100-task run with one aggregate manifest |
| P1 | Improve final-answer schema/source checks in prompt or validator | bad final answer, missing fields | High | Medium | Medium | prompts/eval validation | judged local completions improve |
| P2 | Targeted anti-bot/login handling and profile support | anti-bot/login block | Medium | High | Medium | browser harness/cloud browser | rerun affected tasks |

Adjust the table based on the actual run. Do not keep placeholder priorities if the evidence says otherwise.

### Report Layout

Use this structure for `$OUT/real-v8-eval-analysis.md`:

```markdown
# real_v8 Codex Cloud Eval Analysis

## Executive Summary

One paragraph with strict score, generous score, local completion rate, and the biggest failure mode.

## Scorecard

| Metric | Count | Rate |
| --- | ---: | ---: |
| Judged success | N | N% |
| Judged partial | N | N% |
| Judged failure | N | N% |
| Not judgable/provider | N | N% |
| Unjudged | N | N% |
| Local completion | N | N% |

## Failure Mode Chart

Paste the bar chart or table from `failure-modes-bars.txt` / `failure-modes-table.md`.

## Failure Mode Details

For each mode:
- Count and affected task IDs.
- Representative examples.
- What went wrong.
- Why it likely happened.
- Code/setup area implicated.

## Root Causes

Group root causes by provider, eval runner, browser harness, model behavior, and prompt/schema validation.

## Ranked Fix Plan

Priority table with impact, effort, confidence, validation plan.

## Rerun Recommendations

List which tasks should be rerun, at what parallelism, and why.

## Caveats

Call out 25-way provider stress, judging uncertainty, missing artifacts, and any unjudged tasks.
```

## Rerun Policy

Do not blindly count all failed processes as agent failures.

Recommended reruns:

- Rerun provider timeouts at `--concurrency 10` or lower.
- Rerun empty/invalid provider response failures; classify as transient unless repeated.
- Rerun suspicious local completions with full model I/O only if you need diagnosis, not just scoring.
- Do not rerun clear task-quality failures unless you are measuring best-of-N behavior.

Targeted rerun example:

```bash
TASK_ID=13
RERUN_OUT="$OUT/reruns/task-$TASK_ID"
mkdir -p "$RERUN_OUT"

LLM_BROWSER_PROVIDER_MAX_RETRIES=5 \
LLM_BROWSER_RECORD_MODEL_IO=true \
./target/debug/browser-use-terminal \
  --state-dir "$RERUN_OUT/state" \
  dataset-run-codex real_v8 \
  --task-id "$TASK_ID" \
  --model gpt-5.5 \
  --max-turns 80 \
  --python-timeout-seconds 180 \
  --max-attempts 1 \
  --concurrency 1 \
  --browser-mode cloud \
  --run-id "task-$TASK_ID-rerun"
```

## Final Report Template

A useful final eval report should include:

- Run id and artifact root.
- Provider, model, browser mode, parallelism, max turns, max attempts, timeout settings.
- Number of tasks launched, completed manifests, missing manifests.
- Local runner counts: local `ok`, provider failures, session failures.
- Judged counts: success, partial, failure, not judgable.
- Failure-class counts.
- Rerun policy and which tasks were rerun.
- Known caveats, especially provider stress from `--concurrency 25`.
- Cost/usage summary when reliable.

Example headline:

```text
real_v8 Codex cloud run
Artifact root: /tmp/real-v8-codex-cloud-YYYYMMDD-HHMMSS
Parallelism: 25
Model: gpt-5.5 via Codex login
Browser: Browser Use cloud

Local completed: N / 100
Judged success: N / 100
Judged partial: N / 100
Judged failure: N / 100
Not judgable/provider: N / 100
```
