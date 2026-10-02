# TS goal completion audit

Completed 2026-09-12 Asia/Shanghai (2026-09-11 UTC) against the current worktree.
The goal is to complete the
first two experiment steps and implement the third step. Its first step is
recorded-evidence reward comparison, the second is controlled learner validation,
and the third is the BitRouter serving integration. The later request separately
authorizes constructed coding tasks using the local Codex subscription as the
pilot data source. This completes the requested data-acquisition alternative;
it does not make those tasks naturally sampled historical usage or establish live
routing benefit. Linear TS, LinUCB/OFUL and a publishable live-benefit claim are
not prerequisites for this first TS implementation.

The interim audit treated naturally sampled history as an overall blocker. That
was too restrictive after the user's explicit request to generate real tasks.
The interim record is preserved in the audit directory; this interpretation
supersedes it. The required first-step methods and evidence are unchanged, and
comparative superiority is allowed to be inconclusive rather than assumed.

The [machine-readable audit](experiments/acp_ts_goal_audit_20260911.json) links
current source hashes, matching passing tests, prior experiment artifacts and
new native checks. It does not overwrite earlier studies or their source scopes.

## Requirement-by-requirement result

| Requirement | Evidence and assessment |
|---|---|
| Compare heuristic, scalar and rubric feedback with a sealed independent reference on recorded ACP sessions | Completed as the user-authorized controlled real-task pilot: frozen prefixes, sealed model references, same-model scalar/rubric comparisons, a documented narrower heuristic, disagreements, failures and missingness. The small selected sample does not establish general comparative superiority. Natural-history calibration remains a separate study. |
| Generate a few controlled real coding scenarios using the local Codex subscription | Complete: the initial pilot retains five families, a follow-up prefix and a startup failure; the subsequent study adds four fresh families. Records, model references, comparisons and all failed attempts are preserved. |
| Do not replace evidence with an evaluator's new tool execution or pre-session reward target | Production judge consumes the frozen projection, exposes no tools and uses fixed templates with post-hoc obligation selection. Study workers separately perform authorized tasks; that execution is not a product eval capability. |
| Immutable checkpoints, append/revision semantics and family dependence | Checkpoint tests, effective-observation tests and the real appended-session pilot preserve old prefixes and replace effective contributions. Missing load/resume continuity stays explicit. |
| Manual/automatic/off modes and inspectable rubric history | Current regular tests and the current native TUI controls cover manual corrections, automatic evaluation, history, mode fences, trial dispatch and restoration. |
| N/A, unknown and observed failure remain distinct | v2 structural tests plus fresh real-task comparisons cover forbidden checks, environment-blocked validation, review-only delivery and obligated review repair. Unknown bounds are not a complete learner reward. |
| Durable judge jobs, retry/idempotency, version changes and separate overhead | Current job/scheduler/cost tests pass. A real completed v1 job returned unchanged through v2, with no new request IDs or history changes. Incomplete old contracts are fenced before new model attempts. |
| TS with an explicit continuous likelihood and versioned prior | Normal-Inverse-Gamma sampling, source-stable numerical draws and configured quality/resource gates are implemented. The learner source and default configuration still match the sealed v3 simulation. |
| Cold start, delayed/missing/revised feedback, fork correlation and drift | The v3 publication study retains 576 scenario/strategy/seed runs with negative findings. It does not claim universal safety: long incorrect provisional labels caused harmful adoption in 7/16 TS runs; delayed feedback constrained throughput. |
| Sticky complete policy-block assignments and pending limits | Current admission, revision and concurrency tests pass. Existing sessions keep their assignments; unknown/pending observations cannot justify expansion. |
| Multiple blocks, dependencies, publication, withdrawal and recovery | Current service/runtime tests preserve independent blocks, track lineage and dependencies, fence stale plans and restore supported baselines. Current v2 full native campaigns observed automatic adoption, quality rollback and subsequent baseline dispatch. Their fixture response/price/label provenance remains explicit. |
| Actual request/resource accounting, fallback and late settlement | Current gateway/resource/cost tests cover managed-request membership, unions, late revisions, unknown fallback cost and independent judge overhead. Subscription trials retain unknown monetary cost rather than treating stored zero subtotals as free usage. |
| User can operate the loop through the coding TUI | Current native checks cover code edits, recorded tests, automatic scoring, manual feedback, candidate dispatch, operator withdrawal and terminal restoration for the maintained Codex and Claude adapters. Both current v2 full adoption/monitoring campaigns passed with default parameters. |
| Prove real multi-model cost or quality improvement | Not part of the requested third-step code implementation and not established by these artifacts. It requires a separately designed live experiment and adequate independent outcome evidence. |

## Current-version integration check

Only eight source/test files differ from the preceding full native acceptance
snapshot; they are the rubric/evidence/judge/job contract changes and their tests.
The all-feature suite for those changes passed 3,377 tests with 22 skipped, plus
five doc tests, Clippy with warnings denied, and formatting/diff checks.

This audit additionally ran four opt-in maintained-worker terminal checks on the
current source. All four passed. They use local fixed upstream replies and
rubric labels, with real worker processes and tool execution; they do not add
subscription spending or judge-accuracy evidence.

The first Codex coding check emitted a nextest leaked-handle warning despite
passing its terminal-restoration assertions. A subsequent process inspection
found no remaining worker at the pinned fixture executable paths; an unrelated
operator worker was identified by its earlier start time and separate working
directory and left untouched. One isolated rerun passed without the warning.
The first warning is retained, not relabeled as a clean first run or a fixed
product defect. This does not prove the warning cannot recur.

Both full native publication campaigns then passed on the unchanged v2 source,
with no leaked-handle warning in that run:

| Worker | Trial sessions | Baseline / candidate | Post-adoption monitoring sessions | Recorded initial, trial and monitoring model requests |
|---|---:|---:|---:|---:|
| Codex ACP | 240 | 212 / 28 | 8 | 498 |
| Claude ACP | 246 | 210 / 36 | 8 | 1,020 |

Both observed real code writes and executed tests, automatic adoption, automatic
quality rollback, subsequent actual baseline dispatch while still Automatic,
and terminal restoration. Default exploration parameters were retained. The
request column is the test's scoped coding-request count, not an all-purpose
API spend total or a measurement of live savings. These are fixed-upstream
serving tests, separate from the subscription model-generated task pilot.
After both test handles completed, no pinned fixture worker or temporary fixture
database remained. The full run took about 22.2 minutes; the two test cases ran
concurrently.

## Completion boundary and future research

The controlled reward pilot and learner experiments have evidence for their
stated scopes. The serving implementation passed current-version full native
campaigns and the final artifact audit. The requested goal is complete under the
user's later authorization to generate real coding tasks for the first pilot.
This does not require a positive comparative-superiority finding, and no such
finding is assumed from the small pilot or synthetic learner study.

Natural-history calibration requires its own eligible corpus. The current
operator database contains 40 request records, zero legacy trajectory rows and
no ACP canonical tables; those request records cannot reconstruct missing
conversation or tool evidence. That future study is not represented as complete
and no operator traffic was enrolled. No new source defect requiring a change
was established by this audit.
