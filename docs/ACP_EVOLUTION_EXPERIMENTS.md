# ACP checkpoint evolution experiments

Status: controlled learner and coding-terminal acceptance completed, followed by
the user-authorized real-task reward pilot and rubric v2 checks on 2026-09-11.
Natural-history calibration requires a separate ACP corpus. No operator traffic
was enrolled and no live cost reduction has been measured. The
[goal audit](ACP_TS_GOAL_AUDIT.md) distinguishes the requested controlled pilot
from the future natural-history study.

The final current-v2 native campaigns also passed: Codex used 240 trial sessions
(212 baseline, 28 candidate), Claude 246 (210 baseline, 36 candidate); each then
completed eight monitoring sessions with automatic quality rollback and verified
actual baseline dispatch while Automatic. These fixed-upstream serving results
are recorded in the [final goal audit](ACP_TS_GOAL_AUDIT.md), separately from the
subscription task pilot and earlier source snapshots.

## Recorded-evidence assessment

The available operator database was inspected read-only. It had no ACP canonical
tables and its existing trajectory tables contained no rows. No usable historical
ACP dataset has been located. Consequently, blinded model reference review,
dialogue-heuristic comparison, scalar-judge comparison, evaluator calibration,
and real false-success/missed-failure rates remain unmeasured.

The scoring and leased-judge pipeline is tested with explicit controlled ACP
fixtures. These are mechanics tests, not historical recordings, human reference
labels, or measurements of judge accuracy. Model-generated reference labels must
still be sealed before comparator outputs are exposed when a dataset is available.
The [recorded-evidence review protocol](ACP_RECORDED_EVIDENCE_REVIEW_PROTOCOL.md)
defines the dataset cutoff, reference sealing order, paired comparisons and
unknown/cost reporting. It is preparation for that study, not a completed review
or a substitute dataset.

## Controlled real-model evidence pilot

The user subsequently authorized [real Codex subscription tasks](ACP_SUBSCRIPTION_PILOT.md)
to collect a separate pilot dataset. Five task families, six coding turns and one
startup failure now have seven sealed model references and completed scalar and
production-rubric comparisons. This yielded measured evaluator disagreements and
concrete rubric/capture issues; it does not supply natural-usage calibration or
routing savings. The original historical-data limitation above remains scoped to
that study, not a claim that no real-model pilot has now been measured.

The subsequent [rubric v2 comparison](ACP_RUBRIC_V2_HOLDOUT.md) adds four fresh
constructed families. It confirms revised review responsibility, exclusion of
forbidden executable checks, unknown validation under a fixed-environment blocker,
and retained scoring for actual review repair. Both production contracts evaluated
the same frozen prefixes after independent factual references were sealed.
The v2 source passed 3,377 all-feature tests, Clippy, formatting and doc checks.
These results do not replace the earlier v1 pilot or measure natural-usage
accuracy, causal request credit or multi-model routing savings.

## Controlled allocator experiment

The [machine-readable result](experiments/acp_checkpoint_ts_controlled_v3.json)
contains configuration, source digest, all seed-level outcomes, and learning
curves. Reproduce it with:

```sh
cargo run -p bitrouter --example checkpoint_ts_simulation -- \
  --seeds 16 --sessions 1200 --output result.json
```

There are 16 paired workload seeds, 1,200 synthetic sessions per run, five
scenarios and three strategies. Each strategy sees only its selected arm's
feedback; the simulator keeps unselected potential outcomes solely for evaluation.
The fixed strategy and Thompson allocator share quality-withdrawal and pending
limits. Withdrawal is irreversible. Promotion recommendations are recorded but
are not enacted, so this is not an experiment of the deployed publication loop.

The experiment replans every 16 synthetic arrivals, uses 2,048 posterior draws,
and raises the candidate-session quota to the 1,200-session horizon. Those
settings are included in the artifact; they do not prove the behavior of the
default 200-session quota or the durable service's closed enrollment cohorts.

Baseline quality is approximately 0.94. A useful candidate has the same mean
quality and lower synthetic cost/latency. A harmful candidate has mean quality
0.55. Delayed feedback arrives 48 sessions later. Revised feedback initially
mislabels a harmful candidate as 0.98, then corrects it after 64 arrivals. Drift
changes a useful candidate into a harmful one halfway through the workload.

The table reports averages across seeds. Cost uses a synthetic USD scale; it is
not API spending and does not include real judge overhead.

| Scenario | Guarded fixed 10% quality | Fixed cost | TS quality | TS cost | TS candidate sessions |
|---|---:|---:|---:|---:|---:|
| Useful candidate | 0.9400 | 1120.99 | 0.9400 | 816.81 | 591.12 |
| Delayed feedback | 0.9400 | 1151.07 | 0.9400 | 1155.49 | 69.94 |
| Quality drift | 0.9339 | 1147.83 | 0.9327 | 996.93 | 313.12 |
| Harmful candidate | 0.9373 | 1195.29 | 0.9375 | 1195.47 | 8.12 |
| Incorrect labels revised later | 0.9338 | 1188.30 | 0.9250 | 1170.78 | 46.31 |

The baseline-only strategy averages quality 0.9401 and synthetic cost 1200.87.
Every run respects the four-pending-candidate limit and, after feedback drains,
retains exactly one effective contribution per synthetic session. Both adaptive
and fixed strategies withdraw the candidate in all 16 harmful, revised and drift
runs. These reactive results do not establish that harmful promotion was avoided.

Two negative results materially constrain the next step:

1. TS costs slightly more than fixed allocation under the long-delay condition;
   sparse feedback and enrollment timing limit adaptation. The closed-cohort
   serving scheduler must be measured separately.
2. Incorrect provisional labels cause harmful promotion recommendations in
   **9 of 16 TS runs**, versus zero for guarded fixed allocation. Faster learning
   amplifies plausible but wrong feedback before corrections arrive. Replacing
   old labels repairs the posterior afterward, but cannot undo prior exposure.
   Automatic promotion is not validated by this experiment. Feedback maturity,
   evidence reliability and delayed-confirmation policies need explicit testing;
   the artifact must not be presented as proof of safe automatic evolution.

The drift case also produces one harmful promotion recommendation for guarded
fixed allocation. The enacted publication experiment below separately measures
adoption, monitoring and rollback. This historical allocator-only study cannot
support claims about their behavior.

## Enacted publication experiment, learner v1

This initial experiment uses the production `ControlState::reserve_trial`,
`apply_plan`, `apply_monitoring` and posterior routines. Adoption actually routes
subsequent synthetic sessions to the candidate; rollback actually restores the
baseline and stays latched. Existing trial observations and deployment monitoring
remain separate. The [result](experiments/acp_checkpoint_ts_publication_v1.json)
includes source digests, every seed, transition times and learning curves.

```sh
cargo run -p bitrouter --example checkpoint_ts_publication -- \
  --seeds 16 --sessions 1200 --output result.json
```

There are nine scenarios and four strategies: baseline only, guarded fixed 10%
exploration, Thompson allocation, and Thompson with adoption monitoring disabled.
All use paired potential outcomes; unselected outcomes are available only to the
experiment's measurement code. The fixed comparator changes only the exploration
probability range, retaining the same promotion and withdrawal rules. The monitor
ablation retains original-trial revalidation, including late corrections.

The learner uses its default prior, 8,192 posterior draws, four-pending-candidate
limit and 200-candidate trial quota. Cohorts close after 16 members or the pending
candidate limit, and reconciliation occurs every 16 synthetic arrivals. This
clock is not a mapping to real session duration or the daemon's polling interval.
Feedback drains after the 1,200-arrival horizon without creating more traffic;
transition times and the state at the horizon distinguish late publication.

The delay scenario draws feedback delays uniformly from 8 to 96 arrivals. Two
revision scenarios initially score the harmful candidate at 0.98, then correct
it after respectively 8–64 or 96–384 arrivals. No feedback grace period is fitted
to those delays. Missing feedback never returns for 10% of sessions; a separate
scenario returns assessments with unknown quality for 10%. Related forks share
quality noise in groups of four and receive independent native-session draws:
mixed-arm families are excluded, rather than treated as independent evidence.

These are in-memory publication experiments with synthetic prices and outcome
labels. They do not exercise canonical capture, a database, daemon reload, a real
judge or a maintained coding harness. Their scope complements the canonical and
serving fixtures; it does not replace full capture-to-publication acceptance.

All 576 runs completed after the cross-cohort admission fix described below.
Every run respects the four-pending and 200-trial-candidate limits, retains
exactly one membership per arrival, and never resumes a rolled-back experiment.
The following are TS averages over 16 seeds; adoption/withdrawal are counts of
runs with enacted transitions, not recommendations. Prices are synthetic USD.

| Scenario | Mean quality | Mean cost | Adoptions | Withdrawals | Mean harmful sessions after adoption |
|---|---:|---:|---:|---:|---:|
| Useful candidate | 0.9401 | 498.98 | 16/16 | 0/16 | 0 |
| Harmful candidate | 0.9375 | 1195.49 | 0/16 | 16/16 | 0 |
| Delayed feedback | 0.9399 | 1020.46 | 16/16 | 0/16 | 0 |
| Short correction delay | 0.9349 | 1191.51 | 0/16 | 16/16 | 0 |
| Long correction delay | 0.9116 | 1144.59 | 16/16 | 16/16 | 66 |
| Quality drift | 0.9321 | 872.75 | 16/16 | 16/16 | 24 |
| Permanently missing feedback | 0.9403 | 1199.03 | 0/16 | 0/16 | 0 |
| Assessed but unknown quality | 0.9403 | 1175.96 | 0/16 | 0/16 | 0 |
| Correlated forks | 0.9404 | 1124.69 | 0/16 | 0/16 | 0 |

Useful candidates graduate after an average 157 arrivals under TS versus 289
under fixed exploration. With 8–96-arrival feedback delays, TS graduates in all
16 runs at 768–1,120 arrivals; fixed exploration graduates in none within the
workload or subsequent feedback drain. These are workload-specific liveness
results, not a prediction of elapsed time on actual coding sessions.

The monitoring ablation isolates a concrete effect: after quality changes at
arrival 600, enabled monitoring withdraws at arrival 624 in every seed. Without
monitoring, all 600 remaining sessions use the harmful adopted policy. Mean
quality is 0.9321 with monitoring versus 0.7451 without it. The identical 24-arrival
reaction across these seeds reflects the synthetic effect size and 16-arrival
reconciliation clock; it is not a general rollback-delay bound.

The long correction-delay condition is a decisive negative result: TS enacts a
harmful promotion in **16/16 runs**, versus **5/16** for guarded fixed allocation.
TS serves a mean 66 harmful sessions after adoption before withdrawing. Disabling
the adoption monitor produces the same transitions here: revisions to the
original trial revoke its support before the monitoring stream exposes the
error. Rollback repairs subsequent routing but cannot undo prior exposure.
This experiment does not validate automatic promotion under unreliable judges.
The next algorithm work must test independently justified feedback reliability
and confirmation criteria; choosing a grace period equal to a simulated
correction delay would not establish that protection.

Missing feedback and mixed-arm fork families also expose liveness limits. All
missing/unknown and correlated-fork runs remain unpromoted. Permanent missingness
leaves a mean 2.25 unresolved trial sessions; assessed-unknown sessions resolve
their bookkeeping but still cannot support promotion. Correlated forks exclude
a mean 101.56 mixed-arm families. These outcomes must not be reported as success
merely because harmful adoption is zero. Recovering feedback and an explicit
family-level trial design need further study before those workloads are usable.

## Enacted publication repeat, learner v2

The [v2 result](experiments/acp_checkpoint_ts_publication_v2.json) repeats all
576 runs with `checkpoint-normal-inverse-gamma-ts-v2`, the same paired seeds,
1,200 arrivals and unchanged scenario parameters. This run compiled the sources
recorded by [native validation v11](experiments/acp_checkpoint_native_publication_validation_v11.json),
before the later operator-restoration changes. Its embedded source digests
identify that code; the v1 artifact and results above remain historical.

V2 retains bounded initial exploration until each arm has sufficient independent
quality, cost and latency observations. It fixes starvation caused by an
unmeasured arm's mismatched resource prior in the maintained-worker fixture.
In this synthetic workload the change also delays posterior-driven expansion.
The following are Thompson averages and enacted transition counts across 16
paired seeds. Synthetic costs exclude judge overhead.

| Scenario | Mean quality | Mean cost | Adoptions, including feedback drain | Withdrawals | Mean harmful sessions after adoption |
|---|---:|---:|---:|---:|---:|
| Useful candidate | 0.9401 | 560.14 | 16/16 | 0/16 | 0 |
| Harmful candidate | 0.9373 | 1195.32 | 0/16 | 16/16 | 0 |
| Delayed feedback | 0.9400 | 1189.88 | 1/16 | 0/16 | 0 |
| Short correction delay | 0.9357 | 1193.15 | 0/16 | 16/16 | 0 |
| Long correction delay | 0.9189 | 1159.67 | 7/16 | 16/16 | 16 |
| Quality drift | 0.9321 | 933.91 | 16/16 | 16/16 | 24 |
| Permanently missing feedback | 0.9403 | 1199.47 | 0/16 | 0/16 | 0 |
| Assessed but unknown quality | 0.9403 | 1174.92 | 0/16 | 0/16 | 0 |
| Correlated forks | 0.9404 | 1123.66 | 0/16 | 0/16 | 0 |

Useful-candidate adoption now occurs at a mean 246 arrivals (range 192–320),
compared with 289 for guarded fixed allocation and the earlier v1 mean of 157.
All delayed-feedback runs remain exploratory during the 1,200-arrival workload;
one adopts at arrival 1,265 while draining feedback, with no subsequent candidate
deployment in the experiment. Fixed allocation adopts in none. **V1's delayed
feedback liveness result does not hold for the current learner.**

Long-lived wrong labels still produce **7/16 harmful TS adoptions**, compared
with **5/16** under guarded fixed allocation. Their mean harmful deployment is
16 versus 12 sessions, respectively, averaged over all seeds. All eventually
withdraw. The reduction from v1's 16/16 harmful TS adoptions is not a reliability
guarantee: slower exploration also makes useful adoption slower. The adoption
monitor ablation again cannot reveal these errors before original-trial label
corrections do. Drift monitoring still reduces harmful deployment from 600 to
24 sessions on average under this fixture's timing and effect size.

Missing and assessed-unknown feedback, and correlated fork families, still
prevent all adoptions in their scenarios. Missing feedback leaves a mean 2.3125
unresolved trial sessions; correlated forks exclude a mean 102.4375 mixed-arm
families. The pending cap remains four and the trial-candidate quota remains
200 in every run. These constraints bound exposure but do not demonstrate
usable learning progress. Delay handling, feedback reliability and family-level
allocation remain unresolved research and product issues, alongside validation
against historical ACP evidence and net cost including the judge.

## Numerical sampling stability, learner v3

The default-parameter coding-terminal campaign exposed a numerical stability
issue: effective observation revisions also seeded trial posterior sampling.
In the Claude v5 record, equal posterior summaries produced different Monte
Carlo estimates after a provenance revision. That run remained adopted and
passed its later quality rollback. The Codex v5 run adopted at 153 trials and
then failed its recent-family rollback assertion; its precise withdrawal action
was not captured, so attributing that failure to resampling remains a hypothesis.

A production learning-snapshot regression reproduces changing seeds after an
equal-score assessment revision. Learner v3 now derives common numerical draws
from the experiment and learner version. It preserves full source-revision
publication fences, updates the posterior for actual score corrections, and
retains all existing exploration and quality gates. The controller simulator
calls the same production seed helper. Old v1/v2 plans require revalidation.

The [progress validation record](experiments/acp_checkpoint_ts_stable_draws_validation_v1.json)
pins the source and records the failing regression before repair, 3,373 passing
regular tests, all four maintained-worker terminal smoke/control tests, and
Clippy, documentation, format and diff checks. That record predates completion
of the full campaign. The [final validation record](experiments/acp_checkpoint_ts_stable_draws_validation_v2.json)
adds both completed default-parameter coding publication campaigns and verifies
unchanged numerical sampling across actual provenance-only revisions. The completed
[576-run repeat](experiments/acp_checkpoint_ts_publication_v3.json) follows the
[recorded protocol](experiments/acp_checkpoint_ts_stable_draws_protocol_v1.json).
Its [raw-data verification and analysis](experiments/acp_checkpoint_ts_stable_draws_analysis_v1.json)
checks every scenario/strategy/seed cell, exact source digests, all pending and
cumulative trial caps, session accounting and latched withdrawals. All 144
baseline-only runs exactly reproduce their v2 outcomes; product parameters are
unchanged.

| v3 scenario | TS result across 16 seeds | Interpretation |
|---|---|---|
| Useful candidate | 16 adoptions; mean arrival 246, versus 290 for guarded fixed allocation | Descriptive synthetic sample efficiency |
| Directly harmful candidate | 0 adoptions; 16 withdrawals | Quality guard operates on these observed labels |
| Delayed feedback | 0 adoptions within 1,200 arrivals; 1 after drain at arrival 1,265 | Delay remains a learning-throughput limitation |
| Long incorrect provisional labels | 7 harmful adoptions, versus 5 for guarded fixed allocation; mean harmful deployment 16 versus 12 sessions | Wrong feedback can authorize harmful adoption before correction |
| Quality drift | 16 withdrawals; mean harmful deployment 24 sessions, versus 600 with the adoption monitor disabled | Monitoring limits, but does not eliminate, exposure after drift |
| Missing, assessed-unknown, or correlated-fork feedback | 0 adoptions in each scenario | Unavailable or mixed-family evidence does not satisfy promotion |

Relative to v2, 24 runs change trial-candidate counts and 23 change transition
records. Harmful adoption/deployment counts and horizon/final states match in
every run. These are previously inspected workload seeds, not a new held-out
study, and synthetic costs exclude judge overhead. The historical v2 studies
below retain their original scope and source snapshots. Common draws address
numerical resampling, not inaccurate rubric labels, reward-model misspecification
or the lack of real historical assessment data.

## Delay diagnosis and cohort-size sensitivity

The [diagnostic analysis](experiments/acp_checkpoint_ts_delay_analysis_v1.json)
contains 512 production-controller simulations under a
[protocol recorded before the new runs](experiments/acp_checkpoint_ts_delay_protocol_v1.json).
It compares cohort limits of 16 and 64, guarded fixed allocation and TS, four
scenarios, and two seed ranges. Seeds 0–15 were already available in the v2
experiment; seeds 16–31 were reserved for this comparison. Every preexisting
run field in the 128 repeated batch-16 development runs matches v2 exactly.

The delay regression is primarily an enrollment-throughput problem under these
settings. Batch-16 TS leaves a mean 1,018.375 arrivals unenrolled in the development
workload: 934.5625 wait for the closed cohort's feedback and 83.8125 wait for the
next reconciliation. Only 18.25 candidate sessions are collected on average.
Eleven of the sixteen runs finish with fewer than the 20 candidate families
required for warm-up; even meeting that minimum does not establish promotion.
Late feedback is retained, and these delay runs have no unresolved sessions
after feedback drains. Pending outcomes are never treated as successes.

The reserved-seed results below use 16 runs per cell and 1,200 arrivals per run:

| TS measure | Cohort limit 16 | Cohort limit 64 |
|---|---:|---:|
| Delayed-feedback adoptions within the workload | 1/16 | 16/16 |
| Delayed-feedback adoptions including feedback drain | 3/16 | 16/16 |
| Immediate-feedback mean adoption arrival, all runs adopt | 230 | 267 |
| Harmful-candidate adoptions | 0/16 | 0/16 |
| Long erroneous-label condition: harmful adoptions | 10/16 | 5/16 |
| Long erroneous-label condition: mean trial candidate sessions | 40.6875 | 46.4375 |
| Long erroneous-label condition: mean harmful sessions after adoption | 25 | 9 |

The fixed allocator also improves under delay: in-workload adoptions increase
from 0/16 to 15/16. This isolates a large contribution from cohort mechanics,
rather than establishing an advantage unique to TS. All 512 runs respect the
four-pending-candidate limit and the 200-candidate trial quota. Larger cohorts
delay adoption under immediate feedback and still permit harmful adoption after
incorrect labels. **The product default remains 16.** These synthetic results do
not establish judge reliability or net savings including evaluation overhead.

The simulation now records admission wait reasons and optional decision traces.
It also follows the current controller contract: an open-cohort promotion
proposal is staged without adopting. The larger-cohort runs exercise that fence
22 times. Reproduce selected cases without rerunning the entire suite:

```sh
cargo run -p bitrouter --example checkpoint_ts_publication -- \
  --scenario delayed --strategy guarded-fixed --strategy thompson \
  --seed-start 16 --seeds 16 --sessions 1200 --batch-sessions 64 --trace \
  --output delayed-held-out.json
```

[Wu and Wager](https://arxiv.org/abs/2202.12431) analyze TS that updates from
arriving rewards under independent stochastic delay assumptions; waiting for an
entire cohort is an additional system choice. Their guarantees do not transfer
to our NIG multi-metric model, revised rubric labels or informative missingness.
[Joulani et al.](https://proceedings.mlr.press/v28/joulani13.html) provide broader
background on learning with delayed feedback. A future change to admission or
adaptive cohort size needs explicit outcome-dependent delay and actual-serving
validation; this sensitivity experiment does not implement such a change.

The [delay-study validation record](experiments/acp_checkpoint_ts_delay_validation_v1.json)
also covers the read-only TUI evidence display: usable quality, cost and duration
session groups, and the reviewed experiment's own minimum, including archived
versions. The source snapshot passed 3,372 all-feature tests with 18 skipped;
the two opt-in maintained-worker terminal tests passed separately. Clippy with
warnings denied, documentation tests, formatting and diff checks passed. Those
native tests exercise automatic feedback, a manual correction, a real candidate
dispatch, operator withdrawal and the next request's baseline restoration. Their
model replies and rubric labels remain fixtures; they do not establish judge
calibration or a terminal-driven adoption and quality-rollback campaign.

## Maintained-worker coding acceptance

The opt-in `native::coding` terminal tests use actual maintained workers to write
`native_sum.py` inside the fixture directory and execute two Python unit tests.
The upstream fixture supplies tool calls through each worker's advertised shell
tool; required command permissions are answered through the terminal. The judge
fixture reads the resulting canonical tool observations and cites them for the
post-execution verification rubric. It does not read the repository or execute
the checks itself. Successful single-checkpoint checks establish this connection
for both maintained adapters, without establishing judge accuracy.

The separate `tui_coding_publication_and_rollback` campaign registers its
candidate through the TUI, retains the default 10% initial allocation and all
other bandit settings, and drives fresh coding sessions until the background
scheduler enacts adoption. Subsequent controlled implementations make the same
unit tests fail, exercising quality monitoring, automatic rollback and the next
request's supported baseline. Every session's recorded requests remain in its
cost union, including capability-guarded auxiliary baseline calls under a
candidate assignment. Trial identities and numerical outcomes are compared
separately from provenance revisions that can change when capture closes.

These are explicit opt-in serving acceptance workloads with per-operation and
total deadlines. They use controlled model replies, prices, latency and rubric
labels. The [completed validation record](experiments/acp_checkpoint_ts_stable_draws_validation_v2.json)
reports both campaigns passing with the unchanged source snapshot used for the
regular test suite and four maintained-worker terminal smoke/control tests.

| Maintained worker | Trial sessions | Baseline / candidate trials | Monitoring sessions to quality rollback | Recorded initial + trial + monitoring model requests |
|---|---:|---:|---:|---:|
| Codex ACP | 244 | 212 / 32 | 8 | 506 |
| Claude ACP | 205 | 167 / 38 | 8 | 856 |

Both campaigns observe actual automatic adoption, separate monitoring, quality
rollback, the next request's baseline dispatch while Automatic remains enabled,
and terminal restoration. Source revisions following adoption retain identical
posterior summaries, seeds and probability estimates. The first monitoring
session passes its tests; the following seven fail their executed tests.
All policy mutations occur through the TUI or production scheduler. These
results establish the controlled terminal implementation's behavior; they do
not replace historical calibration or measure live routing benefit. The counts
are outcomes of these runs, not promised production sample requirements.

## Operator restoration through the coding terminal

The [restoration validation record](experiments/acp_checkpoint_operator_restore_validation_v1.json)
covers the shared CLI/TUI withdrawal operation, durable reasons, stale-target
rejection and retry after a newer experiment exists. Controlled service tests
retain an adopted parent baseline, unrelated blocks, mode and session assignments.
An assembled pipeline sends an existing session's next request and a fresh
session's request to the strong baseline after a cheap candidate is withdrawn.
The adopted setup in that test is a state fixture, not a measured promotion.

A separate actual-PTY test opens the built coding CLI with a mock ACP agent and
an isolated assembled control service. After a coding turn it navigates the
block evidence, pastes a reason, reviews the baseline, submits the withdrawal,
checks the persisted publication and verifies terminal restoration on exit.
It caught ignored paste events in selector fields; those fields now accept
pasted content without treating pasted newlines as confirmation. Seven focused
checks and the full 3,366-test suite pass; 16 opt-in tests remain skipped in that
full run. This is an incremental terminal/control acceptance result, not a
substitute for historical judge calibration or the complete maintained-worker
evaluation and improvement workflow.

## Fresh sessions and checkpoint assessment history

The [session and history validation record](experiments/acp_checkpoint_tui_session_history_validation_v1.json)
covers two further terminal workflows. New session creates a distinct native
session and retains direct routing and the turn timeout; request-construction
tests also verify the retained model, base URL and no-start flags. Assessment
history opens earlier checkpoint revisions after two recorded coding turns,
preserving the newer selected reward. Service tests cover historical manual
prefill, corrections, retractions and source/owner fences. The full suite passes
3,371 tests with 16 opt-in tests skipped, and Clippy, doctests and formatting pass.
These are actual CLI PTYs with fixture ACP processes and fixed annotations over
captured canonical evidence. They do not establish judge quality or complete the
maintained-worker evaluation and improvement workflow.

## Learner change prompted by testing

The initial fixed-variance normal model never recommended promotion in any of
the 16 useful-candidate runs. Reporting zero harmful promotion without checking
useful-candidate graduation would therefore have been misleading.

The current model uses a normal-inverse-gamma prior. With mean prior strength
`kappa`, mean `m`, and expected observation variance `v`, its variance prior is
inverse-gamma with shape 2 and scale `v`. Family observations update both mean
and dispersion; Thompson draws use the marginal Student-t mean posterior.
The update follows [Murphy's Gaussian conjugate analysis](https://www.cs.ubc.ca/~murphyk/Papers/bayesGauss.pdf).
Quality/log-cost/log-latency independence remains a working approximation.

The useful candidate now reaches promotion recommendations in all 16 TS runs.
A separate deterministic regression case graduates it with 80 observations per
arm, within the default candidate quota. This establishes mechanics and synthetic
liveness, not real-world calibration or distribution-free quality protection.
The revised-label failure above remains an open issue.

## Implementation evidence and remaining integration

Focused tests cover immutable evidence citations, rubric missingness, continuous
posterior updates, family clustering, cold start, pending holds, recent-quality
withdrawal, durable joint assignments, reconnect/restart idempotency, concurrent
admission, mode epochs, preserved control arms after adoption, label replacement,
append/retraction/deletion invalidation, and publication-time canonical fences.

Judge tests cover cached recovery, exactly-once selected revisions, bounded
network retries, lease ownership, manual correction precedence, source deletion,
and mode changes before dispatch and during a model call. Tool execution is
disabled in the SDK's streaming and non-streaming server-tool loops when the
request selects no tools. A regression caught and fixed lease renewal suspending
a model future while it held the only database connection.

The assembled model pipeline now admits authenticated, recorded canonical
sessions before named-policy selection and records actual execution attempts and
settlement. Isolated app tests use real local HTTP upstreams and controlled ACP
recordings to exercise sticky choices, candidate preset defaults, named-policy
tool guards, scope/ambiguity handling, fallback cost uncertainty and A-to-B-to-A
reload fencing. These are serving integration fixtures, not historical coding
sessions or evidence that promotion is safe.

The [serving validation record](experiments/acp_checkpoint_serving_validation_v1.json)
pins the checked source files. The full all-feature nextest run passed 3,304
tests with 12 skipped; documentation tests passed 5 with 1 ignored. All-feature,
all-target Clippy passed with warnings denied, along with formatting and diff
checks. The eight serving integration tests also cover request-capability
fallback, session override precedence, mode fences, streaming completion and
disconnect, and preserving original metering evidence after a rejected replay.
An adoption regression preserves the original route of preexisting sessions
that never participated in the experiment. These checks establish behavior at
the pinned source snapshot; they do not complete the remaining product scope.

The gateway inventory now records unresolved requests as well as bound execution.
The local capture controller acknowledges coverage through the serving daemon's
control socket before its first event. Coverage revisions fence resource snapshots
and learning publication, including changes without a canonical content append.
The controlled fixtures exercise fully priced stopped checkpoints, pending prompt
boundaries, late settlement, continued sessions, unresolved identities, unrelated
sessions, retroactive acknowledgement rejection, runtime changes, replay after
capture closes, and inherited fork cost union. A fork regression also established
that a response stored in the child must close the parent's request through a
bounded reference and remain a deletion dependency.

The newer [gateway validation record](experiments/acp_checkpoint_gateway_validation_v2.json)
pins this implementation snapshot: **3,312 tests passed, 12 skipped** in the full
all-feature nextest run. Documentation tests passed 5 with 1 ignored. All-feature,
all-target Clippy passed with warnings denied; formatting and diff checks passed.
The earlier serving record remains evidence for its own source snapshot.

Coverage is scoped to the registered authenticated controller's gateway traffic.
The handshake does not prove that every real harness call propagates that identity.
These tests use controlled canonical events, isolated databases and local HTTP;
they do not establish real session completeness, judge accuracy, promotion safety
or live cost savings. Unknown prices, incomplete fallback charges, unresolved
membership and interrupted capture still prevent a complete resource observation.

The daemon-owned scheduler now discovers stopped prefixes, freezes checkpoints
under feedback-epoch fences, executes durable automatic judge jobs, refreshes
resources and reconciles against live routes. Local CLI/control-socket actions
expose status, feedback modes, block registration, learning inspection and
reconciliation. Controlled tests cover restart deduplication, manual mode without
a model, no implicit historical bulk judging, epoch retirement, continued
sessions, manual correction precedence, concurrent workers, committed-receipt
recovery while off and cancellation of an uncertain model attempt. These fixtures
return unknown rubric values deliberately; they measure scheduling mechanics,
not evaluator accuracy or promotion outcomes.

The [scheduler validation record](experiments/acp_checkpoint_scheduler_validation_v3.json)
pins this newer source snapshot: **3,320 tests passed, 12 skipped** in the full
all-feature nextest run. Documentation tests passed 5 with 1 ignored. All-feature,
all-target Clippy passed with warnings denied, both evolution CLI help commands
matched the reference, and formatting and diff checks passed. Earlier validation
records remain evidence only for their own source snapshots.

## Local coding TUI feedback controls

### Maintained adapter acceptance

The [maintained-worker terminal record](experiments/acp_checkpoint_native_tui_validation_v1.json)
adds actual PTY coverage with the built coding CLI, both maintained adapters and
their real worker executables. All mode, evaluator, manual correction, candidate
registration and withdrawal actions use terminal controls. Fresh sessions run
under the unmodified candidate defaults until a candidate request is actually
dispatched. Each trial must enter learning with quality, cost and latency
available. After operator withdrawal, the same native session's next turn must
dispatch the baseline while evolution remains Automatic; the operator then
switches Off and exits with terminal settings restored.

The final captured run used eight trial sessions with Codex and 13 with Claude. These
are observed allocations from one run per adapter, not sample-efficiency
estimates. The upstream replies, rubric labels and prices are fixtures. This
acceptance does not establish judge accuracy, net savings, real coding/tool
outcomes or the full TUI adoption and quality-triggered rollback campaign. That
broader campaign and the historical reference review remain incomplete.

The final snapshot passes 3,372 ordinary tests with 18 opt-in tests skipped;
the two worker terminal tests above pass separately. Clippy with warnings denied,
doctests and formatting also pass. The remote-menu regression now checks that an
off-screen action remains searchable. Concurrent scheduler validation uses two
independent connection pools against one persistent SQLite file and preserves
the one-judge/one-revision assertions. Both regressions pass 20 repeated runs.

The terminal test exposed a real presentation defect: a bordered two-row menu
heading had no inner row, so applicability instructions and scoring anchors were
invisible. The heading now reserves space for wrapped multiline instructions;
rendered checks cover both 80×24 and 120×40 terminals. The trial observer retries
explicit concurrent-read fences and requires actual numeric feedback, rather
than treating an assigned but unevaluated session as learned evidence.

Using the same adapter/worker variables described below, run the terminal tests:

```sh
cargo nextest run -p bitrouter --all-features --run-ignored only \
  --success-output final \
  -E 'test(maintained_codex_tui_) | test(maintained_claude_tui_)'
```

The explicit `acp_cli::evolution_acceptance` tests launch the pinned
[`codex-acp`](https://github.com/agentclientprotocol/codex-acp) and
[`claude-agent-acp`](https://github.com/agentclientprotocol/claude-agent-acp)
packages with their actual worker executables through the TUI's shared
`SessionHost`. Both use an isolated home, working directory, database, HTTP
gateway and control socket. Inherited environment variables are stripped before
applying the fixture's launch configuration. The test checks each adapter's
package identity/version against the maintained harness pin; it does not install
packages or read the operator's authentication/configuration files.

Run them explicitly with an empty `BITROUTER_API_KEY`, `BITROUTER_TEST_NODE`
pointing to Node, and these paths supplied by the test environment:

- `BITROUTER_TEST_CODEX_ADAPTER`: the pinned package's `dist/index.js`.
- `BITROUTER_TEST_CODEX_WORKER`: its Codex worker executable.
- `BITROUTER_TEST_CLAUDE_ADAPTER`: the pinned package's `dist/index.js`.
- `BITROUTER_TEST_CLAUDE_WORKER`: its Claude worker executable.

```sh
cargo nextest run -p bitrouter --all-features --run-ignored only \
  --success-output final -E 'test(evolution_acceptance)'
```

The external-package tests are ignored by default. A normal full-suite run does
not establish this acceptance. The two continuation tests each make two prompt
turns in one real ACP session. Actual model HTTP ingress must match its canonical execution inventory;
the assignment must stay fixed. The automatic scheduler must select a new
checkpoint after the append and replace the single effective learning sample.
Its cost must match the observed requests included by the resource membership
contract, while judge calls remain separately charged exactly once. The output
compares checkpoint cost with all recorded model requests. After worker shutdown,
the test also requires every gateway request to be represented in the final
resource set, complete known costs or explicit unknowns, and one effective sample.
Another unchanged scheduler pass must make no new judge request.

The earlier v9 snapshot exposed additional Claude calls after prompt completion
at the same canonical watermark. Its timestamp-bound rule recorded them in the
gateway inventory but omitted them from the earlier checkpoint cost. A subsequent
content checkpoint happened to include them; that did not prove final closure.
The resource membership correction below addresses this case without changing
the frozen ACP content.

The upstream model replies and rubric labels are deterministic local fixtures.
This checks protocol translation, capture, identity propagation, automatic
feedback and accounting mechanics. It does not measure model/judge quality or
real spending, and does not exercise terminal interaction, tools, compaction,
forks, reconnection, fallback or promotion through a real worker.

The acceptance exposed a first-request eligibility defect: both maintained
adapters emit command-list notifications before model dispatch. Treating every
`session/update` as earlier execution excluded fresh sessions. Recognized,
schema-valid administrative notifications now preserve admission and remain in
canonical evidence. Other content still excludes late enrollment. In particular,
Codex sends unknown-model metadata warnings as assistant text; the Codex fixture
uses a worker-known model alias to avoid that warning. Warning-triggering custom
aliases remain a documented integration limitation, not a passed acceptance case.

The [adapter validation record](experiments/acp_checkpoint_adapter_validation_v9.json)
pins this snapshot. Both external-adapter tests and the administrative-admission
regression passed explicitly. The full all-feature nextest suite passed **3,353
tests, with 14 skipped**; the two adapter tests account for the two new default
skips. All-target Clippy passed with warnings denied, documentation tests passed
5 with 1 ignored, and formatting/diff checks passed. This record retains the
observed post-stop cost gap rather than treating prefix completeness as final
native-session cost completeness.

### Resource membership and worker-exit accounting

The v9 cost gap is repaired by resource membership `native-head-resources-v2`.
Own-session gateway requests at the stopped canonical head enter a new resource
observation even if admitted after the last content event. Unfinished requests
remain unknown until terminal settlement. The next native prompt advances the
head, so its requests cannot enter the earlier checkpoint. Fork-inherited
segments keep their original timestamp cutoff and exclude later parent work,
including a parent auxiliary call at the same head immediately after the fork.

A reproducing regression first failed with observed cost 28 instead of 56
micro-USD. It now includes the post-stop request, keeps a subsequent pending call
incomplete, incorporates its terminal receipt, and excludes the following turn
from the old checkpoint. The frozen prefix digest remains unchanged. Another
case advances a different session on the shared connection and verifies that an
unresolved post-stop request remains unknown until the next event of the original
native session. A legacy JSON regression verifies that old resource rows remain
unchanged, are excluded from learner costs, and can be refreshed without a new
assessment revision or model call.

The actual-adapter acceptance now closes and reaps each worker before a final
scheduler pass. It compares every model request seen by the local HTTP gateway
with canonical executions and the final resource request-ID set. It verifies
known costs and explicit unknowns, one effective native-session observation, and
an unchanged retry that makes no new judge call. Recorded results for the two
literal-reply turns are:

| Adapter | Gateway coding requests through exit | Final known coding cost, micro-USD | Unpriced requests | Judge cost, micro-USD | Effective session samples |
|---|---:|---:|---:|---:|---:|
| Codex ACP 1.10.0 | 2 | 600 | 0 | 60 | 1 |
| Claude ACP 0.75.1 | 5 | 1500 | 0 | 60 | 1 |

These are controlled protocol and accounting results at the recorded source
snapshot. Prices, model responses and rubric labels are fixture data. They do
not measure evaluator accuracy, natural coding outcomes, promotion safety or
net routing savings. Real-worker tools, fallback, forks, reconnects and candidate
publication remain separate acceptance work.

The [resource-closure validation record](experiments/acp_checkpoint_resource_closure_validation_v10.json)
pins this source snapshot and the failing regression that motivated the change.
The 17 focused tests, including both explicitly enabled real-adapter tests,
passed. The full all-feature suite passed **3,356 tests, with 14 skipped**.
All-feature/all-target Clippy passed with warnings denied; documentation tests
passed 5 with 1 ignored, and formatting/diff checks passed. Earlier records remain
evidence for their own snapshots and are not retroactively relabelled.

### Native publication workload and cold-start correction

The additional `publication_and_rollback` tests open fresh native sessions through
the same maintained workers, gateway and persisted learner. Automatic fixture
rubrics are computed from the recorded ACP evidence. Promotion and withdrawal
must come from scheduler reconciliation; no adoption or learner rows are planted.
The candidate has synthetic token prices one tenth of the baseline and local
response delays of 5 ms versus 250 ms. Initial exposure is explicitly 50%; the
default minimum twenty independent families per arm, quality/noninferiority,
latency, posterior-probability and cumulative candidate limits remain unchanged.
These explicit native publication tests have a separate ten-minute nextest
deadline because they launch hundreds of sequential worker sessions; individual
prompts and evaluations retain their own bounded deadlines. The ordinary suite
keeps its existing timeout.

Every candidate session must actually dispatch the cheap model. A recorded
request-capability guard may retain the strong model for auxiliary work; that
request remains charged to the original assigned arm. Closed native resource
sets and learner costs must equal actual settled request unions. New adopted
traffic must enter a separate monitoring population, leaving the exact trial
evidence unchanged. Fixed low fixture labels then exercise the ordinary quality
monitor, including its minimum independent-family count, and the next native
session must dispatch the restored baseline. These are controlled mechanics
checks, not measurements of judge reliability or natural coding benefit.

An initial run exposed prior-driven cold-start starvation: one cheap baseline
observation reduced allocation before any challenger outcome. A deterministic
regression reproduced 20,000 ppm rather than the configured 100,000 ppm. In the
native workload, one run reached 152 baseline sessions and zero challenger
sessions before the 300-second test timeout. Learner v2 retains bounded warm-up
allocation until both arms have the configured minimum independent metric
evidence. All quality, pending and exposure guards remain active. Old reduced
allocation is retained, and incompatible persisted plans must be revalidated
before new trial admission. A validated promotion plan can be staged while its
cohort remains open or unresolved, allowing that cohort to finish after an
upgrade. Repeated evidence cannot change a recommendation through new random
draws; an already-staged promotion can be enacted once its cohort becomes ready.

The [native publication validation record](experiments/acp_checkpoint_native_publication_validation_v11.json)
records the passing campaigns and the earlier failed runs that motivated the
changes. The Codex campaign passed in 115 seconds. A Claude campaign reached
adoption after 124 trials but hit the ordinary 300-second deadline during the
remaining checks; its separate rerun passed in 274 seconds with the explicit
long-workload deadline. Counts vary with native identities, randomized assignment
and worker timing; these are individual controlled acceptance campaigns.

| Adapter | Trial sessions (baseline / candidate) | Additional low-scored monitoring sessions before rollback | Actual coding requests (strong / cheap) | Judge cost, synthetic micro-USD |
|---|---:|---:|---:|---:|
| Codex ACP 1.10.0 | 76 (37 / 39) | 7 | 85 (38 / 47) | 2550 |
| Claude ACP 0.75.1 | 112 (57 / 55) | 7 | 363 (300 / 63) | 3630 |

Each monitor first received one good adopted session, so rollback had eight
independent observed families and no severe-violation shortcut. The recorded
request totals also include a fresh restored-baseline session. Unchanged
scheduler retries added no judge calls or publications. The Claude result keeps
its strong auxiliary work in candidate costs rather than treating every request
as cheap. Terminal interaction, natural tool workflows, compaction, forks,
reconnection and provider-failure fallback remain separate real-worker acceptance.

The final all-feature suite passed **3,359 tests, with 16 skipped**. All-feature,
all-target Clippy passed with warnings denied; documentation tests passed 5 with
1 ignored, and formatting/diff checks passed. Four external-worker tests remain
explicitly enabled acceptance cases rather than part of the default suite.

Earlier seeded allocator and publication artifacts above remain historical
results for their recorded source digests. The cold-start correction does not
resolve their incorrect-label harmful-adoption result.

### TUI controls

The `/evolution` flow now calls the real local control socket through the
application action port. It exposes feedback modes, judge selection, status,
session checkpoints, manual rubric drafts and existing-block reconciliation.
The review retains a frozen evidence packet, expected assessment revision and
submission identity. Evidence inspectors return to the review; failed actions
preserve the coding prompt and scoring draft. Manual scoring supports explicit
unknowns, permitted non-applicability, custom continuous scores, citations,
explanations, violation evidence and a reward preview before saving.

Controlled tests exercise the action port over an isolated daemon socket,
including same-input retries, stale corrections, non-applicable mandatory
criteria, invalid numeric scores, verification claims without tool evidence,
mode-preserving judge changes and the operations-only boundary. They also cover
a continued native session: a successful save of an earlier prefix must be
reported as stale and excluded from current learning. This caught a misleading
receipt that previously treated `selected_on_submission` as proof that the
assessment still applied to the current prefix. The response now includes the
fresh effective state. No judge is called during these manual review tests;
their scores are explicit fixture inputs, not reference labels for actual tasks.

The [TUI validation record](experiments/acp_checkpoint_tui_validation_v4.json)
pins this source snapshot: **3,325 tests passed, 12 skipped** in the full
all-feature nextest run. Documentation tests passed 5 with 1 ignored. All-feature,
all-target Clippy passed with warnings denied; formatting and diff checks passed.
This validates action-port, IPC and state-machine behavior, not an actual
interactive-terminal acceptance run or historical-session evaluator accuracy.

## Adoption monitoring implementation

New native sessions admitted to an adopted block while evolution is enabled now
receive separate durable monitoring membership. Controlled tests cover reconnect
and restart stability, no retroactive enrollment after off/on, recent-family
missingness, fork clustering and supported human violation vetoes. An isolated
canonical fixture validates monitoring reconstruction, rejects a stale snapshot
after a score correction, enacts withdrawal under publication fences and checks
that the next route uses the baseline. Original trial sample counts do not grow
from monitoring observations. A still-supported original trial revision records
revalidation without changing the adopted policy's dependency revision.

Factoring admission for the publication experiment exposed another defect:
resolved assessments can still contain unknown quality. Closing such a cohort
previously left its last positive exploration rate available even when the new
plan explicitly held allocation at zero. Admission now honors that hold; a
regression verifies that correcting the unknown feedback can resume the trial.

The first 16-seed publication run then failed its pending-limit assertion in the
assessed-unknown scenario. A follow-up regression reproduced the cause: three
unknown candidates from an earlier cohort plus two new reservations could exceed
the four-pending limit. Admission now carries forward the incomplete count in
the last validated plan and includes reservations made since that plan. The
regression permits exactly one additional candidate after one of four unknown
scores is corrected, rejects the next reservation, and resumes after the rest
are corrected. The completed publication artifact is generated after this fix;
the failed experiment is not counted as successful validation.

The [monitoring validation record](experiments/acp_checkpoint_monitoring_validation_v5.json)
pins 55 source files and the completed publication artifact. The full all-feature
nextest run passed **3,329 tests with 12 skipped**. Documentation tests passed 5
with 1 ignored; all-feature/all-target Clippy passed with warnings denied, along
with formatting and diff checks. Earlier validation records apply only to their
own source snapshots. The original pending-limit failure and its reproducing
regression are retained as output digests in the new record.

## TUI candidate creation

The local TUI now builds typed candidate experiments with several related route
changes, manual/configured-judge feedback, a rationale and an explicit assumption
about other blocks. A live preview includes physical model/fallback paths and
the default trial/quality limits. Balanced accounts are enumerated in configured
order with their rotation semantics identified; credentials and endpoint URLs
are excluded from the presentation. The register action binds the preview to
owner, evaluator contract, control/mode revisions and actual route dependencies.
Registration preserves mode and leaves the current session's assignment intact.

Controlled tests exercise creation through the actual local control socket,
retrying a committed registration after a mode change, admitting a fresh native
session into the created block, changed routes/settings, altered and cross-owner
previews, overlapping matchers, evaluator-catalog refresh, disconnected drafts
and manual/candidate draft exclusion. No model is called by these tests.

IPC testing exposed a precision defect: the default JSON parser changed a log
prior mean by one least-significant bit. This invalidated both untouched preview
fingerprints and comparison with definitions restored from persistence. A
regression reproduced the exact bit difference; the application now enables
exact floating-point round trips instead of weakening the digest checks.

The [candidate validation record](experiments/acp_checkpoint_candidate_validation_v6.json)
pins 58 source files: **3,335 tests passed with 12 skipped** in the full
all-feature nextest run. Documentation tests passed 5 with 1 ignored; all-target,
all-feature Clippy passed with warnings denied, and formatting/diff checks passed.
This covers the action port, control socket, drafts, routing snapshots and native
admission. It does not establish interactive-terminal acceptance, judge accuracy,
or a new routing-outcome comparison. Earlier simulation and validation artifacts
remain evidence for their own source snapshots.

## Judge overhead accounting

The assembled pipeline now records each judge request's reservation, dispatch,
upstream attempts and terminal settlement. The reservation and the job attempt
commit together. Owner-local CLI and TUI status join current metering evidence
to a union of these request IDs, with job/session totals and a retry subset.
Incomplete totals remain unknown. These figures describe metered estimates,
not invoices or measured counterfactual savings.

Controlled HTTP fixtures exercise invalid rubric responses, exhausted retries,
fallback, rejection before dispatch, successful cached reuse, cancellation,
late authoritative receipts and confirmed non-charge. Additional tests cover
atomic reservation rollback, owner isolation, legacy missing metadata and the
TUI action port through the actual local control socket. No external model is
called and no operator traffic is enrolled by these checks.

A new regression reproduced a false zero-cost conclusion for a rate-limited
request: metering's legacy normalization inferred zero usage and marked its
origin as provider-reported. The first cost reader incorrectly accepted that
row as complete. Attempt metadata now preserves the pipeline's original usage
provenance from the metering event. The same case remains unknown until an
authoritative receipt supplies evidence; a terminal rejection before any
upstream attempt remains a provable zero.

Review also found that source deletion removes judge jobs to erase cached
content. Costs are therefore retained in a separate, content-free owner ledger,
with an indexed request-owner binding. The deletion test confirms that the job
and cached-content lifecycle does not remove already incurred spend. Costs
without current job details are an included subset, not a second total. Deleted
legacy jobs without reservations cannot be reconstructed.

The [judge-cost validation record](experiments/acp_checkpoint_judge_cost_validation_v7.json)
pins 62 source files. The full all-feature nextest run passed **3,343 tests with
12 skipped**. Documentation tests passed 5 with 1 ignored; all-feature/all-target
Clippy passed with warnings denied, and formatting/diff checks passed. The record
retains the reproduced normalized-rejection failure and the final validation
output digests. Earlier validation and simulation artifacts still apply only to
their own source snapshots. These checks provide no new judge-accuracy or
routing-savings measurement.

## Sequential experiment revision acceptance

The CLI and local TUI now start another experiment for an existing policy block.
The reviewed definition preserves its matcher set and inherits the supported
baseline, or explicitly rebases to configured selectors after live dependencies
change. The new version has independent learner/cohort state. Prior experiments,
session assignments, monitoring memberships and registration receipts remain
available. TUI history inspection and reconciliation target a specific version.

Controlled cases exercise multi-generation baseline inheritance, archived
evidence reconstruction, stale previews, changed routing contracts, restart,
legacy enrollment records, incomplete capture, idempotent replies and concurrent
control-generation fencing. Local HTTP pipeline fixtures check the models
actually executed before and after inherited adoption withdrawal. Local control
socket tests drive revision drafts, previews, registration and history inspection.
These are isolated fixtures; they do not call external models or activate
operator traffic. Earlier trials' adoptions are arranged in some integration
fixtures to isolate revision behavior, not established as real coding success.

An archived experiment cannot enroll new sessions or gain a new adoption. Late
corrections can still withdraw its adoption and dependent versions, retaining
the last supported baseline. An already rolled-back version must advance its
policy revision again if an earlier inherited baseline is subsequently withdrawn;
otherwise another block can retain a stale declared dependency. Publication
records identify the affected experiment as well as its policy revision.

The [revision validation record](experiments/acp_checkpoint_revision_validation_v8.json)
pins 66 source files. The final all-feature nextest run passed **3,352 tests with
12 skipped**, including the reproduced second-ancestor withdrawal regression.
Documentation tests passed 5 with 1 ignored; all-feature/all-target Clippy passed
with warnings denied, and formatting/diff checks passed. The record retains the
failing regression output digest and final validation output digests. Earlier
validation and simulation records remain evidence for their own snapshots.

The revision checks do not establish evaluator accuracy, interaction-free block
outcomes, net cost savings or interactive-terminal acceptance. New versions'
outcomes are not reused as archived monitoring samples. The original late
incorrect-label promotion failure remains an open algorithm limitation.

The v8 revision record is historical. Subsequent native publication, terminal
history and operator-restoration records above cover additional integration
work; each pins its own source snapshot. The maintained-worker coding campaign
above completes terminal adoption and quality-rollback acceptance. Remaining
evidence gaps include actual historical ACP assessment calibration and measured
net routing benefit including judge overhead. A correlation-only cost snapshot
cannot authorize promotion. The
revised-label simulation failure remains an algorithm limitation, and evaluator
accuracy remains unmeasured while historical ACP data is unavailable.
