# Real-model input corpus, 2026-10-14

This evaluates current Goal input interpretation and durable effects using OpenRouter reasoning and native typed requests. The workflow state is explicitly labeled fixture data. No real harness is dispatched, and a saved pause is not proof of stopped execution. All labels began as Codex-authored candidates. q07's temporary-hold control meaning and q03's missing-evidence query clarification now have explicit human decisions.

## v1: original labels, terminal failure

- Run `d856d377-424e-4032-a254-1dbd27010c8f`, 244.671 seconds: all 25 inputs ran through both model paths. 50 lanes recorded; 14 lanes had 20 effect differences. [Log](2026-10-14-input-corpus-v1-nextest.log), [full durable results and original proposals](2026-10-14-input-corpus-v1-durable-results.json), [summary](2026-10-14-input-corpus-v1-summary.json), [original labels](2026-10-14-input-corpus-v1-labels.jsonl), [source manifest](2026-10-14-input-corpus-v1-source.sha256).
- q03: both models request missing failure evidence. The fixture has two running tasks, no observed failure or failure log. Its candidate `clarification=false` is disputed; the original result remains failed against that annotation. No invented failure explanation is acceptable.
- q07: baseline saved pause; typed-upgraded reasoning saved stop. The original label expected stop. The user subsequently chose temporary hold = pause with resumption intent. The current label records the change and its source; this does not rewrite v1's result.
- q12: typed-upgraded reasoning added requirements to retain partial results while pausing. Retention is already default lifecycle behavior.
- q15: both models acknowledge unscoped agreement without a specific clarification for the pending Decision. Neither grants permission.
- q16–18: permission request fixture serialized incomplete JSON rather than `PendingPermissionSnapshot`; original hash checks succeeded but typed decoding failed. Models received no valid original request and declined to grant or explain it. This is a fixture fault, not proof of a production reader failure or poor permission classification.
- q25: both paths update Safari instructions but also invent task run control. Existing run intent already permits reproduction; a work request alone cannot resume paused execution.

The initial strict Clippy attempt failed on invalid test command fields added while compilation was in progress. Those fields were corrected, and the original seven-state seed test then passed. The later typed-permission seed construction also initially failed because the ACP fields type is non-exhaustive; it now uses its supported Default construction. These compile failures are preserved separately from model results. No check was bypassed.

Exact trusted-state comparison also found that 21 of 24 model-input pairs matched; q21/q23/q24 differed only in the prior unresolved input's `original_input.accepted_at_ms`. Each lane had independently accepted the same fixture message at its actual wall clock time. q22 makes no model request. The corrected seed supplies the same fixture acceptance time through GoalService in both lanes, and the runner compares the actual reasoning request's trusted state after each pair. Original v1 results remain unchanged.

## Current correction and evidence boundary

PRD and implementation spec now say that temporary holds default to pause, explicit ending means stop, and ambiguous targets still require clarification. A task-scoped hold includes downstream dependents and preserves independent branches. Runtime feedback distinguishes saved intent from confirmed execution behavior.

The model instructions also clarify retention, work arrangement versus resumption, and unscoped agreement when a permission Decision is pending. The permission seed now uses actual Rust ACP types; its regression reads the material through the production hash-checking reader and checks the original permission identity and option count before any billed request.

Corpus `expected.answer` is checked against actual production input feedback: a model answer, a saved control/requirements/permission receipt, or a concrete clarification. Raw generated `answer` is retained separately. It does not count a saved receipt as a model answer, verified result, delivered permission, or confirmed pause. The historical v7 q02 generated-answer observation remains unchanged.

New source is not covered by v7's full green checks. The next run must preserve original proposals, runtime refusals, saved effects and UI feedback separately. Model semantic metrics must not count a guard-blocked unsafe proposal as correct. Deterministic q22 withdrawal is outside model classification denominators. Remaining sequence, direct-control calibration, real Goal/harness and provider-loop gates remain open.

## v2: human hold preference and corrected material

Run `44f34229-67bb-437e-a9d6-9a42df4f0449`, 229.254 seconds, terminal exit 100. All 50 lanes are recorded; 48 match candidate effect annotations and q03's two lanes still disagree on query clarification. The run remains failed. All 24 actual model-input pairs now have exactly equal trusted state; q22 uses the deterministic withdrawal path without model work.

[Log](2026-10-14-input-corpus-v2-nextest.log), [durable snapshots, raw proposals and UI feedback](2026-10-14-input-corpus-v2-durable-results.json), [per-lane metrics](2026-10-14-input-corpus-v2-metrics.json), [source/config/annotation manifest](2026-10-14-input-corpus-v2-source.sha256).

Both q07 paths save task:left/pause and the controlled set left + join; requirements stay at revision 1, root intent remains Run, and the independent right task is unchanged. This proves saved scope and intent, not actual harness cancellation or resumption. q12 now retains results without a new requirement version. q15 names the pending Decision and offered choices without consent. q16/q18 save the actual offered one-time/persistent choices; q17 only explains the original tool request. q25 changes Safari and its downstream requirements without invented run control or independent Chrome changes.

The seven-state fixture regression `3e2e4fd7-521f-4b54-80e4-4b4d6d0e5228` passed (0.323 seconds), including original receipt times, hash-checked typed permission material, stale permission refusal and zero model work. Strict workspace/all-target/all-feature Clippy exited 0; fmt/diff and all 124 manifest files pass. Full workspace/doc/distribution checks are tracked separately until their terminal results.

These native classification numbers are against draft semantic labels, with only 24 samples per binary field. They are not human-gold accuracy or a production performance claim:

| Native field | TP / FP / FN / TN | Precision | Recall |
| --- | --- | --- | --- |
| query | 10 / 1 / 1 / 12 | 10/11 | 10/11 |
| modify | 3 / 4 / 1 / 16 | 3/7 | 3/4 |
| permission response intent | 3 / 0 / 0 / 21 | 3/3 | 3/3 |

For defined control actions, q06/q13/q14 have ambiguous or ineligible targets and are excluded from action scoring but retained in raw evidence. Across the other 21 inputs, native action selection exactly matches 15. Pause is TP4/FP2/FN0/TN15, while Run is TP0/FP4/FN1/TN16; no Stop-positive sample remains after the human q07 change, so its precision/recall is undefined. The five known control targets match 5/5; this does not prove invalid-target classification or permission authority. In particular, native q01 selects hypothetical pause and permission replies select run, while reasoning keeps actual saved effects bounded. q09's native pause differs from the requested resumption. These raw errors are not counted as correct because downstream reasoning/guards prevented them.

All 24 typed cases upgrade to reasoning (24/24). Each path asks a clarification on 8/24 model inputs. Single-run median input-to-persisted-result times are 4,031 ms baseline and 4,581 ms typed-upgrade; configured ledger estimates total 286,597 and 308,810 micro-USD respectively. Neither is an invoice or vendor benchmark. Per-lane reasoning provider-reported cost is preserved separately; DecisionResponse still drops native provider cost/ID fields. There are no held accounting reservations at the end of these inputs. This sample supports retaining the upgrade path and does not calibrate a direct-control threshold.

## First full-check attempt: not green

Workspace run `1e5ce52d-bbe2-489f-99ba-65af9c8624fd`, 495.968 seconds, terminal exit 100: 1,784 passed (14 slow), three tests timed out at the unchanged 300-second bound, 24 skipped, and 2,828 not executed after failure. The two Goal resource cases and native background load/resume timed out. [Original log](2026-10-14-input-corpus-v2-workspace-nextest.log).

The concurrently started doc check exited 1 because a ring rlib was temporarily absent; it subsequently existed again. [Doc failure](2026-10-14-input-corpus-v2-doctests.log). Its following distribution check did not start. Inspection confirms these test processes invoke Cargo to build Rust protocol fixtures in the same target directory, and the owned process tree showed several waiting fixture builds alongside the doc build. Build contention is a supported explanation, not a source-code pass. No timeout or assertion is increased or weakened.

The next owned pipeline serially rechecks those three cases, then runs the complete workspace, doc and distribution checks. Terminal evidence is required before changing this status. [Format](2026-10-14-input-corpus-v2-fmt.log), [diff](2026-10-14-input-corpus-v2-diff.log) and [124 source hashes](2026-10-14-input-corpus-v2-hashes.log) already pass.

The original three cases passed their serial recheck, run `0a0b8308-8f8e-48a9-8c0c-9f59138fafb2`, 134.613 seconds. [Log](2026-10-14-input-corpus-v2-serial-recheck.log). The following full run `ba9ba2b7-b5aa-4350-b520-028b9e2ddf7b` still exited 100: 1,780 passed, one active-branch test failed with a Goal revision conflict, 24 skipped and 2,834 not run. [Full result](2026-10-14-input-corpus-v2-workspace-serial-nextest.log). Doc/distribution did not execute after this failure.

The active-branch fixture's positive permission reply did not handle a known rejected head CAS while background accounting was changing the same Goal. Its other input/control commands already use a bounded eight-attempt rejection loop. The corrected permission call retries only a structured Conflict using the exact same command, operation ID, requirements version 2, Decision ID and offered option. Unknown acceptance/timeout is not retried. It asserts the accepted original operation and version, and retains stale permission, independent branch, stop and delivery checks. No production source changed in this correction, so the v2 real-model evidence remains applicable to production behavior.

That case passed in source124 v3, run `6d3853bb-a9c5-4694-9364-ca107a907c01`, 112.756 seconds. [Log](2026-10-14-input-corpus-v3-active-edit-nextest.log), [v3 source manifest](2026-10-14-input-corpus-v3-source.sha256). A new complete pipeline collects all failures with no-fail-fast and performs the other checks sequentially. Its terminal status is still pending; prior failures remain recorded.

## Subsequent human review of q03

The user explicitly accepts a query clarification when the current Goal has no failure evidence: ask which attempt or log failed, avoiding an invented cause. This applies to the existing q03 state and does not authorize pause, stop, resumption or requirement changes. Its sole expected-effect change is clarification=false to true; query/control/permission/scope labels are unchanged.

The exact [v2 labels](2026-10-14-input-corpus-v2-labels.jsonl) are preserved and match the original recorded SHA-256. A [retrospective review](2026-10-14-input-corpus-q03-human-review.json) verifies both original q03 lanes have query-only clarification, unchanged requirement version/run intent, and no saved control or permission. The other 48 lanes and annotations are unchanged. Thus all 50 captured effect lanes fit the subsequently reviewed annotations. This is neither a fresh model/source run nor a rewrite of v2's failed test exit. Native semantic errors, 24/24 upgrades, sequence evaluation and real harness execution remain open.

## Mixed query/control correction: fresh v3 failure

The [fresh actual v3 run](2026-10-14-input-corpus-v3-nextest.log), `ac1c667c-4340-42cb-80a9-5bf6ef1378ec`, executes 26 inputs in both lanes: 52 captured results, 261.736 seconds, exit 100. [Original results](2026-10-14-input-corpus-v3-durable-results.json), [exact original labels](2026-10-14-input-corpus-v3-labels.jsonl), [summary](2026-10-14-input-corpus-v3-summary.json); production source is the mixed-query-closed-permission-v2 manifest. No runtime processing errors were recorded. Three effect differences are solely against clarification=false: both q05 lanes and baseline q25.

Both q05 models preserve the explicit current Goal pause and clarify absent failure logs, without new requirements or permission. q25 baseline preserves the known Safari reproduction instruction, independent Chrome work and absence of resume; it asks for missing diagnostic evidence and states the cause is unknown. Those query clarifications fit PRD 7.5 and the implemented mixed-intent distinction. q05/q25 remain proposed annotations; this inference from the approved spec is not a new human review. The current annotation allows optional query clarification while retaining the exact control, scope and forbidden-effect checks. Original failed results are not rewritten as a passing run.

The first added q26 duplicated q05's Chinese input/state. It is retained in the original v3 evidence and is not an additional independent quality example. The current q26 is an English variant. Only evaluation data changed after the 54-case local green; the new 134-file v3 source manifest includes the revised annotations. A fresh actual v4 run is in progress; static/full checks, input sequences, calibrated direct-control thresholds and commercial harness delivery remain incomplete.

## Fresh v4: actual scope error, unresolved

[v4 actual run](2026-10-14-input-corpus-v4-nextest.log), `627cad26-d97f-49b7-8f0a-ee814177d868`, records all 52 lanes in 243.558 seconds, exit 100. [Original results](2026-10-14-input-corpus-v4-durable-results.json), [exact labels](2026-10-14-input-corpus-v4-labels.jsonl), [metrics](2026-10-14-input-corpus-v4-metrics.json), [summary](2026-10-14-input-corpus-v4-summary.json). The 25 model pairs receive identical trusted state; deterministic q22 makes no model request. No runtime processing errors were recorded.

Baseline q25 actually changes the independent Chrome task and versions all three tasks to 2. The model replaces the Goal objective and adds an implementation constraint to Chrome while saying to preserve its investigation. That broadening is an incorrect saved effect, not a labeling issue. The natural-language Safari reference was not bound as an explicit task scope before mutation. Existing `/task` scope guards do not cover this case. The original Safari-only scope and forbidden independent changes remain required; the label is not relaxed. Typed-upgraded q25 keeps the expected branch scope in this run, which does not prove reliable target matching.

All other 51 durable effect lanes fit current annotations, including both mixed query/pause variants. The raw native query matrix is 11 true positives, one false positive and one false negative; modify is three true positives, six false positives and one false negative. Permission presence is three true positives with no false positives/negatives in this small candidate set. All 25 typed calls upgrade to reasoning. These are descriptive proposed-label metrics, not calibrated general accuracy or direct-control safety evidence.

Baseline/typed lane p50 input-to-persisted-finish times are 3821/4364 ms; p95 are 6908/7817 ms, using nearest-rank across 25 model samples per lane. Configured model-token estimates total 297014/320432 micro-USD; reasoning provider-reported costs total 0.297010/0.312855 USD. Typed provider-reported cost is not retained by DecisionResponse. None is proof of a full workflow bill or performance benefit. Current production scope handling needs correction; source is frozen for independent static/full regressions first.
