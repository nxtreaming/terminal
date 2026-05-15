# real_v8 Head-To-Head: 112315 vs 174409

Compared runs:

- Previous: `real-v8-codex-cloud-20260513-112315`
- Current: `real-v8-codex-cloud-20260513-174409`

## Headline

The current run is not cleanly "better". It improved infra completion, but it has real regressions on several tasks.

| Metric | Previous | Current | Read |
|---|---:|---:|---|
| Runner local-ok | 93 | 98 | Current completed more sessions. |
| Runner failed/cancelled | 6 | 2 | Current had fewer mechanical failures. |
| Pending | 1 | 0 | Current finished the manifest cleanly. |
| Reported manual strict | 86 | 81 | Current is worse under the reported strict score. |
| Reported half-credit | 88 | 88 | Quality is roughly flat if partials count half. |

The previous `86` and current `81` are not perfectly apples-to-apples. The current judging pass marked more low-quality completions as partial. Rechecking previous artifacts with the current stricter lens shows the previous run also had hidden partial/fail cases that were not called out in its report.

## Real Improvements

| Task | Previous | Current | Interpretation |
|---:|---|---|---|
| 1 | `Done.`, no useful artifact | Structured Nobel answer with laureates, universities, counts, links | Real improvement. |
| 6 | Provider failure reading a missing downloaded file | Full FERC row/file summaries | Real improvement. |
| 17 | Max-turn failure | Correct Henrico court JSON artifact, but final text still `Done.` | Content improved; finalization still flawed. |
| 52 | `Done.`, no Markdown list | Markdown list with 24 items | Improved from fail to partial; still incomplete because trace found 37 items. |
| 66 | Max-turn failure | 5 of 11 Volusia property matches | Improved from fail to partial. |
| 77 | `Done.`, no analysis | Actual Shopify-unavailable site analysis | Real improvement. |
| 88 | Broken pipe with 237-row partial artifact | 405-row artifact with specialty counts | Improved from fail to partial; still below 40 for several specialties. |
| 98 | Max-turn failure, no PDF | Downloaded Pulaski tax bill PDF plus instructions | Real improvement. |

## Real Regressions

| Task | Previous | Current | Interpretation |
|---:|---|---|---|
| 27 | 103 telecom rows; no obvious icon-as-provider issue in quick check | 102 rows, includes `4G forbindelse ikon` as provider | Regressed field quality. |
| 41 | 727 Didacta exhibitors | Cancelled with 400 exhibitors | Clear regression. |
| 46 | 5 Alcom packages, contract length normalized as `No binding` | 5 packages, contract length `Not specified` | Clear regression. |
| 75 | 178 Dallas surgeon records | Cancelled with no result | Clear regression. |
| 100 | 20 per platform; Galaxus ratings missing | 20 per platform; Galaxus ratings missing, Kaufland images mostly wrong, one product name `Bestseller` | Regressed field quality. |

## Same Or Similar Failures

| Task | Previous | Current | Interpretation |
|---:|---|---|---|
| 5 | 107 product rows with `READ MORE` leakage | 107 product rows with `READ MORE` and impossible placeholder prices | Same class, current somewhat worse. |
| 9 | Wrong HostGenius location/property shape | Empty properties array | Still fail. |
| 21 | All-null Booking rates | All-null Booking rates | Same fail; old report did not flag it. |
| 59 | Missing emails for 2 of 5 | Missing/ambiguous emails for 2 of 5 | Same class. |
| 65 | TechCrunch list includes non-startup/fund/public-company entities | Similar, plus promo/session rows | Same class, current worse. |
| 68 | 20 contacts, 7 not found | 20 contacts, 7 not found | Same class. |
| 72 | Max-turn failure | Final says operator ID unknown | Still fail, but current at least found the wrong surface. |
| 87 | 7 SydneyFoodTrucks rows only | 200 rows, but mostly council supplement and target fields blank | Improved count, still partial. |
| 94 | 22 WCA rows, no contact names, 2 emails | Same | Same partial under stricter rubric. |
| 96 | 10 BuiltFirst rows, AppDirect website/category missing | Same | Same partial. |

## Apples-To-Apples Estimate

If the previous run is judged with the same stricter lens used for the current run, its score is probably lower than the reported `86`.

Observed previous-run hidden issues:

- Task 21 was an all-null Booking output and should be fail.
- Task 5 leaked non-data CTA values and should be partial.
- Task 38 had weak package-name quality and should likely be partial.
- Task 59 had missing contact emails and should be partial.
- Task 65 included non-startup entities and should likely be partial.
- Task 68 had 7 not-found emails and should be partial.
- Task 94 had no contact names and only 2 contact emails and should be partial.

Estimated same-rubric previous score:

| Run | Strict pass | Partial | Fail | Half-credit |
|---|---:|---:|---:|---:|
| Previous, reported | 86 | 4 | 10 | 88 |
| Previous, estimated stricter | ~79-80 | ~9-10 | ~11 | ~84-85 |
| Current, judged stricter | 81 | 14 | 5 | 88 |

So the current run is probably not worse apples-to-apples. It is more likely:

- better at avoiding hard infra/provider failures,
- worse on a few extraction tasks,
- roughly similar or slightly better overall under the stricter manual rubric,
- still nowhere near good enough if strict score is the target.

## Bottom Line

The branch did not produce a clean quality win. It produced an infra win and a mixed quality result.

Most important current regressions to fix first:

1. Task 41: long-pagination cancellation after 400/710 exhibitors.
2. Task 75: long surgeon extraction cancelled with no saved output.
3. Task 46: contract normalization regressed.
4. Task 27 and 100: field-quality regressions from bad card parsing.

Most important enduring quality gaps:

1. Add validators for empty/all-null outputs and explicit required values.
2. Add count/completeness checks before final answer.
3. Add field semantic checks for UI labels, icon alt text, CTAs, and placeholder prices.
4. Add incremental persistence and resume for long directory tasks.

