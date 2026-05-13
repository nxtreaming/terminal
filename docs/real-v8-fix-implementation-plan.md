# real_v8 Fix Implementation Plan

Baseline run: `real-v8-codex-cloud-20260513-112315`
Strict manual score: `86/100`
Half-credit partial score: `88/100`
Near-term target: `90-92/100`
Stretch target: `93-96/100`

This plan turns the eval report into implementation work. It is ordered so each phase can be shipped and measured independently.

## Guiding Rules

- Keep Browser Use cloud fail-closed. No task may silently connect to local Chrome.
- Treat runner `ok` as a quality signal, not just "the model stopped".
- Prefer artifact-first large outputs, but always require a useful final answer pointer.
- Retry recoverable infrastructure failures before counting them against the agent.
- Add validators before adding complex browser runtime behavior.

## Why `Done.` Happens

`Done.` is possible through two different paths:

1. The intended path is the `done` tool. Its schema explicitly requires a `result`, and it can consume a persisted Python final answer via `use_final_answer=true` or `result="__use_final_answer__"`.
2. The surprising path is free assistant text. In `crates/browser-use-core/src/lib.rs`, after every provider turn, if the model returns no tool calls and the assistant text is non-empty, the runtime immediately writes `session.done` with that text:

   ```rust
   if tool_calls.is_empty() {
       if !assistant_text.trim().is_empty() {
           store.append_event(
               &session.id,
               "session.done",
               serde_json::json!({ "result": assistant_text.trim_end() }),
           )?;
           return Ok(session.id.clone());
       }
   }
   ```

That means `Done.` is not always a `done` tool call. In several failed traces, the provider emitted the literal assistant text `Done.` with `tool_call_count=0`, and the runtime accepted it as the final result.

This also explains the runner/manual score mismatch. The protocol reads the latest `session.done.result`, and the dataset runner marks an attempt `ok` when the session status is done, no error exists, and a final result exists. It does not know whether the final result is useful.

There is one more bug: persisted final answer replacement currently lives on the `done` tool path. If Python calls `set_final_answer(...)` but the model later emits free text `Done.` instead of calling `done(use_final_answer=true)`, the runtime does not replace `Done.` with `.final_answer.json`. Task 46 showed this exact pattern: valid output existed, but the manifest final result stayed `Done.`.

Fix:

1. Route free-text completion through the same finalization helper as the `done` tool.
2. Apply persisted final answer replacement on both the `done` tool path and the free-text path.
3. In dataset mode, optionally require the `done` tool and turn free-text endings into a corrective model-visible message.
4. Add a final-answer quality gate so placeholder text without useful artifacts is not a success.

## Phase 0: Preserve Cloud-Only Safety

Status: mostly done, keep it protected.

Code areas:

- `crates/browser-use-cli/src/main.rs`
- `crates/browser-use-core/src/tools/command.rs`
- `python/llm_browser_worker/worker.py`

Work:

1. Keep short `BH_RUNTIME_DIR` paths under `/tmp/lbe/...`.
2. Keep `BU_NAME` short and unique per run/task/attempt.
3. Keep cloud mode fail-closed in the Python worker.
4. Keep local CDP env disabled for `exec_command` inside dataset virtual homes.
5. Add a small regression test or smoke script that scans `bu.log` for:
   - `remote=local`
   - `127.0.0.1`
   - `AF_UNIX path too long`

Acceptance:

- A 3-task cloud canary completes with only `wss://...browser-use.com` browser logs.
- Running dataset tasks must not open local Chrome or show local remote-debugging prompts.

## Phase 1: Make The Dataset Runner Impossible To Hang

Problem:

Task 26 produced valid artifacts but remained pending in the manifest. The scheduler currently waits on `rx.recv()` and only learns about task completion when a worker sends a final result.

Code areas:

- `crates/browser-use-cli/src/main.rs`
- possibly `crates/browser-use-store/src/lib.rs`

Implementation plan:

1. Replace the simple `(task_id, Result<Value>)` channel with a `DatasetWorkerEvent` enum:

   ```rust
   enum DatasetWorkerEvent {
       AttemptStarted {
           task_id: String,
           attempt: usize,
           session_id: String,
           paths: DatasetTaskPaths,
       },
       AttemptHeartbeat {
           task_id: String,
           attempt: usize,
           session_id: String,
       },
       AttemptFinished {
           task_id: String,
           result: Result<Value>,
       },
   }
   ```

2. Track active work with an `ActiveDatasetTask` map keyed by task ID:

   ```rust
   struct ActiveDatasetTask {
       task_id: String,
       attempt: usize,
       started_at: Instant,
       last_heartbeat_at: Instant,
       session_id: Option<String>,
       paths: Option<DatasetTaskPaths>,
   }
   ```

3. Replace blocking `rx.recv()` with `rx.recv_timeout(Duration::from_secs(1))`.
4. Add CLI flags:
   - `--task-timeout-seconds`, default around `3600`
   - `--task-idle-timeout-seconds`, default around `900`
5. On timeout, record a runner result instead of leaving the task pending.
6. Add best-effort cleanup:
   - mark session stopped if a session exists,
   - stop owned browser-harness daemon by `BU_NAME`,
   - terminate Python worker processes scoped to the task virtual home if possible.
7. Add artifact salvage before marking a timeout as failure.

Artifact salvage:

1. Inspect the attempt directory for:
   - `artifacts/.final_answer.json`
   - `outputs/result.json`
   - non-empty files under `outputs/`
2. If a useful result exists, emit:

   ```json
   {
     "ok": true,
     "runner_status": "salvaged",
     "error_type": "runner_timeout_salvaged"
   }
   ```

3. If no useful result exists, emit:

   ```json
   {
     "ok": false,
     "error_type": "runner_timeout",
     "error": "task exceeded timeout without usable output"
   }
   ```

Tests:

- Unit test `summarize_dataset_manifest` with salvaged status.
- Unit test artifact salvage from `outputs/result.json`.
- Unit test artifact salvage from `artifacts/.final_answer.json`.
- Integration test with a fake provider that never sends completion and confirm no pending tasks remain.

Acceptance:

- A task cannot stay pending forever.
- A valid task-26-style output is recorded as salvaged.
- A timed-out task without artifacts becomes a normal failed session in the manifest.

Expected score impact:

- `+1` immediately from task 26 false pending.
- Major reliability improvement for long evals.

## Phase 2: Make `Done.` Insufficient For Dataset Success

Problem:

Tasks 1, 52, and 77 were runner-successful but manually failed because the final answer was only `Done.` and no matching deliverable existed.

Code areas:

- `crates/browser-use-cli/src/main.rs`
- `crates/browser-use-core/src/lib.rs`
- `prompts/dataset-case-user.md`
- `prompts/browser-agent-system.md`
- `python/llm_browser_worker/worker.py`

Implementation plan:

1. Add a shared core finalization helper:

   ```rust
   fn finalize_session_result(
       store: &Store,
       session: &SessionMeta,
       requested_result: &str,
   ) -> Result<String>
   ```

   This helper should:
   - trim the requested result,
   - load `.final_answer.json` when present,
   - replace placeholder results with the persisted final answer when valid,
   - append the normalized `session.done` event.

2. Use the helper from both:
   - the `done` tool handler,
   - the no-tool-call assistant text path.
3. In dataset mode, add an option like `require_done_tool` or `strict_final_answer`.
4. When strict mode is enabled and a provider returns free text with no tool calls:
   - if the text is useful, either accept it with `answer_quality: useful_inline`, or ask the model to call `done`,
   - if the text is placeholder, reject/retry.
5. Add a `DatasetAnswerQuality` helper:

   ```rust
   enum DatasetAnswerQuality {
       UsefulInline,
       UsefulArtifactPointer,
       Placeholder,
       Missing,
   }
   ```

6. Classify final results as placeholder if they are:
   - `Done.`
   - `done`
   - empty
   - very short and not a number-only answer for a count task
7. If the final answer is placeholder, inspect artifacts.
8. Accept placeholder only when:
   - `outputs/result.json` exists and parses, or
   - `.final_answer.json` points to an existing output file, or
   - requested screenshot/output files exist and match the task ask.
9. Otherwise mark `ok: false` with:

   ```json
   {
     "error_type": "invalid_final_answer",
     "retry_classification": "bad_output"
   }
   ```

10. Update prompts:
   - For large outputs: write to `/home/user/outputs/result.json`.
   - Final answer must include path, record count, and one sample row or short summary.
   - `Done.` is not acceptable unless the task explicitly asks for no answer.

Tests:

- Free text `Done.` with valid `.final_answer.json` is replaced.
- Free text `Done.` with no artifacts fails or retries in dataset mode.
- `Done.` with no artifacts fails.
- `Done.` with valid `outputs/result.json` passes but gets `answer_quality: artifact_only`.
- Count-only answer like `3` can pass when the task asks for a count.
- Invalid JSON artifact fails.

Acceptance:

- Tasks like 1, 52, and 77 would no longer be false positives.
- Tasks like 46 can still pass because valid artifacts exist.

Expected score impact:

- No direct score gain unless paired with retry, but removes false optimism.
- With retry enabled, potential `+3`.

## Long-Tail Failure Fixes

These are the remaining fixes after the main categories of `Done.`, missing downloads, broken pipe, and max-turn failures. They are mostly quality and strategy problems.

| Task | Failure shape | Likely root cause | Fix | Priority |
| --- | --- | --- | --- | --- |
| 9 | Returned area/location pages instead of individual property listing URLs. | The agent satisfied the semantic topic but missed the requested granularity. | Add validator for "individual listing" tasks: each row must have a property/detail URL, property name/address, and must not be a search/category/location page. Retry with validator feedback. | High |
| 17 | Henrico court records task hit max turns. | Court portal workflow needs a structured search strategy and more compact evidence collection. | Add court-record playbook: inspect form/network, search exact company plus variants, capture case number/party/case type/judgment/date/source, write partial JSON after each query. Retry max-turn with higher budget or resume-from-artifacts. | Medium |
| 66 | Volusia property appraiser task needed 10 names and hit max turns. | Name-by-name portal search is too slow and did not checkpoint enough partial progress. | Add batch property-portal playbook: one normalized search per input name, direct endpoint/form POST when possible, per-name result object even for no-match, partial JSON after every name, validator requires all 10 inputs represented. | High |
| 72 | North Dakota DMR operator ID task hit max turns. | The lookup should probably be API/search-index first, not manual browsing. | Add operator-ID playbook: search official datasets/CSV/ArcGIS endpoints, normalize company names, require an operator ID pattern and source URL before finalizing. | Medium |
| 76 | Spanish Berlin restaurant task included a non-Spanish option. | No constraint check before final recommendation. | Add local recommendation validator: every candidate must satisfy cuisine, location, party size, time/date availability, and source evidence. Reject candidates whose extracted cuisine contradicts the task. | Medium |
| 87 | Food truck task requested 200 records but output only 7. | Large directory extraction stopped after visible-page scraping. | Add large-directory playbook: discover API/sitemap/pagination first, estimate total count, batch output, dedupe, and refuse finalization below the requested count unless documented exhaustion is proven. | High |
| 96 | Local business scrape missed website/category fields. | The first extraction pass was accepted without required-field completion. | Add required-field retry pass: identify rows with missing fields, revisit only those detail pages, fill website/category or write explicit unavailable evidence. | High |
| 98 | Pulaski tax bill PDF hit max turns. | Download workflow for tax portals needs endpoint discovery and PDF validation. | Add tax-bill playbook: parcel/address lookup, inspect network for PDF endpoint, save PDF under outputs, validate nonzero PDF, final answer includes path and `learnings`. | Medium |
| 100 | Galaxus task returned wrong-category products. | Product relevance was not validated against "dietary supplement". | Add product-category validator: title/category/URL must mention supplement, vitamin, mineral, protein, omega, creatine, etc.; reject books/skincare/general wellness false matches. | High |

Cross-cutting implementation:

1. Add validator feedback to retries instead of just failing the task.
2. Add per-task validator overrides for the known real_v8 hard cases.
3. Teach the prompts that if the requested count is explicit, a much smaller output is a failed extraction unless the final answer proves source exhaustion.
4. For portal tasks, write partial artifacts early so a max-turn retry can resume instead of restarting.

Expected score impact:

- The high-priority long-tail validators plus retry should recover `+3` to `+6` points across repeated runs.
- Getting beyond `93/100` likely requires these task-specific validators/playbooks, not just infrastructure fixes.
- Getting beyond `96/100` probably requires better site-specific automation for brittle government portals and large open-web directories.

## Phase 3: Improve Retry Classification

Problem:

`--max-attempts 2` was set, but all tasks ran only once. Several recoverable failures were not classified as retryable.

Code areas:

- `crates/browser-use-cli/src/main.rs`
- provider code if max-turn failure is surfaced there

Implementation plan:

1. Replace boolean retry classification with a reason enum:

   ```rust
   enum RetryDecision {
       DoNotRetry { reason: String },
       RetrySameBudget { reason: String },
       RetryHigherBudget { reason: String },
       RetryWithValidatorFeedback { reason: String },
   }
   ```

2. Mark these as retryable:
   - `Broken pipe`
   - provider stream disconnected
   - missing downloaded file under task workspace
   - `No such file or directory` for expected artifact reads
   - transient cloud browser disconnects
3. Mark `agent exceeded maximum provider turns` as budget exhaustion.
4. For budget exhaustion:
   - second attempt gets increased `max_turns`, or
   - second attempt receives a compact "continue from previous artifacts" instruction.
5. Preserve retry history in the manifest.
6. Surface retry counts in `dataset_report`.

Tests:

- `Broken pipe (os error 32)` retries.
- Missing downloaded file retries once.
- `agent exceeded maximum provider turns` gets `RetryHigherBudget`.
- Permanent errors still do not retry.

Acceptance:

- Task 88-style broken pipe should get a second attempt.
- Task 6-style missing file should get a second attempt.
- Max-turn tasks should either retry with more budget or fail with `budget_exhausted`, not generic provider failure.

Expected score impact:

- `+1` to `+2` from infrastructure retries.
- `+2` to `+4` if budget retries rescue some max-turn tasks.

## Phase 4: Add Dataset Output Validators

Problem:

Manual failures included wrong granularity, too few rows, missing fields, and wrong product category.

Code areas:

- `crates/browser-use-cli/src/main.rs`
- new file such as `datasets/real_v8.validators.json`
- optional new module in `crates/browser-use-cli/src/dataset_validation.rs`

Implementation plan:

1. Add generic validators:
   - JSON parses if task asks for JSON or structured output.
   - Required fields are present and mostly non-empty.
   - Screenshot files exist and are non-empty if requested.
   - URL fields exist if requested.
   - Minimum row count is met when task asks for a specific count.
2. Add task-specific overrides for known hard cases:
   - task 87 expects about 200 records,
   - task 100 expects dietary supplements only,
   - task 9 expects individual listing URLs, not location pages.
3. Run validators before final `ok` is set.
4. If validation fails and attempts remain, retry with validator feedback.
5. If validation fails after final attempt, mark:

   ```json
   {
     "ok": false,
     "error_type": "validation",
     "validation_errors": [...]
   }
   ```

Tests:

- Missing requested screenshot fails.
- Too few rows fails when task has explicit count.
- Required field missing fails.
- Valid artifact passes.
- Validator feedback appears in retry prompt.

Acceptance:

- Tasks 9, 87, 96, and 100 would be caught automatically.
- Runner score becomes closer to manual score.

Expected score impact:

- `+2` to `+4` with retry.
- Better measurement even when score does not improve.

## Phase 5: Harden Download And File Artifact Handling

Problem:

Task 6 failed while reading a downloaded JPG path that was missing from the task downloads directory.

Code areas:

- `python/llm_browser_worker/worker.py`
- browser-harness integration helpers
- `crates/browser-use-core/src/tools/files.rs`

Implementation plan:

1. Define one downloads directory per task attempt:

   ```text
   $LLM_BROWSER_OUTPUTS_DIR/downloads
   ```

2. Configure browser downloads to land there when possible.
3. Emit download metadata:

   ```json
   {
     "browser_url": "...",
     "suggested_filename": "...",
     "local_path": "/home/user/outputs/downloads/...",
     "exists": true
   }
   ```

4. If a browser download is not available locally, provide a model-visible recoverable tool error.
5. Avoid fatal provider termination for normal file-read misses inside task outputs.
6. Add a helper for copying page assets/downloads into outputs before referencing them in final answers.

Tests:

- Downloaded file is written under outputs/downloads.
- Missing file read returns model-visible error.
- File path with spaces and non-ASCII characters is handled safely.

Acceptance:

- Task 6-style missing JPG path should not crash the run.

Expected score impact:

- `+1` likely.
- Reduces noisy provider failures.

## Phase 6: Add Extraction Playbooks

Problem:

The remaining misses are mostly task-strategy misses, not browser connectivity misses.

Code areas:

- `prompts/browser-agent-system.md`
- `prompts/dataset-case-user.md`
- maybe a new skill/playbook under repo docs or prompts

Playbooks to add:

1. Large directory extraction:
   - discover API/sitemap first,
   - estimate total records,
   - write partial batches to files,
   - validate count before final answer.
2. Property/tax/court portals:
   - prefer official search endpoints,
   - save search criteria and result page screenshots,
   - preserve parcel/case IDs and source URLs.
3. Product category tasks:
   - validate category relevance before including rows,
   - reject obvious wrong-category records.
4. Maps/local lead tasks:
   - use pagination and duplicate checks,
   - stop only after requested count or documented exhaustion.

Acceptance:

- Prompts push agents toward bounded extraction and validation instead of open-ended browsing.
- New tasks should write progress artifacts before long loops.

Expected score impact:

- `+2` to `+5` over repeated runs.

## Phase 7: Automate Manual Judging Artifacts

Problem:

Manual judging worked, but the results were not automatically persisted as a reusable JSONL dataset.

Code areas:

- new script under `scripts/`
- docs/runbook updates

Implementation plan:

1. Add a script that creates judge packets from a manifest:

   ```bash
   scripts/build-eval-judge-packets.py <manifest> <out-dir>
   ```

2. Add a JSONL schema:

   ```json
   {
     "task_id": "1",
     "verdict": "pass|partial|fail",
     "runner_status": "success|provider_failed|hung_pending|salvaged",
     "failure_modes": [],
     "notes": "",
     "evidence_paths": []
   }
   ```

3. Add an aggregation script:

   ```bash
   scripts/summarize-eval-judgments.py judgments.jsonl
   ```

4. Generate:
   - strict score,
   - half-credit score,
   - failure taxonomy,
   - Markdown report skeleton.

Acceptance:

- Future reports do not require hand-copying subagent outputs.

Expected impact:

- No direct score gain.
- Much faster iteration and more consistent scoring.

## Rollout Order

1. Phase 1: runner timeout, heartbeat, and salvage.
2. Phase 2: final answer gate.
3. Phase 3: retry classifier.
4. Phase 4: validators.
5. Phase 5: download handling.
6. Phase 6: extraction playbooks.
7. Phase 7: judging automation.

## Verification Plan

For non-TUI changes:

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
```

For any TUI-impacting changes:

```bash
scripts/verify-terminal-ui.sh
```

Eval verification sequence:

1. Run the 3-task cloud canary.
2. Scan all `bu.log` files for local CDP signatures.
3. Run `real_v8` with `--concurrency 25`.
4. Confirm no pending tasks remain in the manifest.
5. Build judge packets and manually judge.
6. Compare against the `86/100` baseline.

## Success Criteria

Minimum successful patch set:

- No task can remain pending forever.
- Placeholder final answers are not accepted as quality successes.
- Recoverable provider failures retry.
- The eval still runs cloud-only.
- Strict manual score improves or measurement becomes stricter without regressions.

Target successful patch set:

- Strict manual score reaches `90/100` or higher.
- Runner manifest and manual score differ by less than 3 points.
- No local Chrome windows or local remote-debugging prompts appear.
