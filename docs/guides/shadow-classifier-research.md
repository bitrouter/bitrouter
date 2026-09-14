# Shadow classifier research protocol

BitRouter's classifier is an evidence producer, not a routing authority. Route
Context V3 separates task family, next-step role, progress, and risk so that a
learned component can improve semantic recall without hiding policy or safety
decisions. Phase 1 evaluates candidates offline only; deterministic risk rules,
signed policy, and runtime guards remain authoritative.

## Candidate ladder

Evaluate the following in order and keep every earlier candidate as a control:

1. the compiled deterministic scorecard, reported as an uncalibrated heuristic;
2. frozen multilingual embeddings with per-class prototypes;
3. a frozen multilingual encoder with a regularized linear head;
4. the same encoder with a two-layer MLP only if the linear head underfits;
5. a distilled static or tiny encoder after the target taxonomy and data split
   are stable.

This ordering is intentional. SetFit demonstrates that sentence embeddings plus
a small classification head can work in label-scarce and multilingual settings,
while Model2Vec provides a path to much smaller CPU inference. Neither result
removes the need for task-specific held-out evaluation, calibration, OOD tests,
or resource measurements. RouteLLM and Universal Model Routing motivate
cost/quality routing evaluation, but Phase 1 does not learn the model-selection
policy itself.

Primary references:

- [SetFit: Efficient Few-Shot Learning Without Prompts](https://arxiv.org/abs/2209.11055)
- [RouteLLM: Learning to Route LLMs with Preference Data](https://arxiv.org/abs/2406.18665)
- [Universal Model Routing for Efficient LLM Inference](https://openreview.net/forum?id=ka82fvJ5f1)
- [Model2Vec reference implementation](https://github.com/MinishLab/model2vec)
- [Calibrated Selective Classification](https://arxiv.org/abs/2208.12084)

## Data protocol

Group examples by task or session before splitting so adjacent turns from one
trajectory cannot cross boundaries. Commit three disjoint sets:

- training: fit prototypes or classifier parameters;
- calibration: fit temperature/thresholds and abstention/OOD policy;
- sealed evaluation: run once for candidate selection.

The checked-in classifier fixtures are a frozen contract and smoke-sized
evaluation slice, not training data and not sufficient evidence for production.
The bake-off rejects identical split digests, but digests alone cannot prove
that the underlying records are disjoint. Dataset construction must also audit
request/session membership before sealing each split.

Labels should come from coarse user outcome feedback plus high-value targeted
review. Spend labeling effort on disagreements, low-margin examples, OOD
clusters, costly mistakes, and under-covered language/phase slices. Preserve
the original label, reviewer provenance, and taxonomy version instead of
silently rewriting historical labels.

## Submission contract

Run the built-in baseline and export the frozen case commitments:

```bash
bro workflow-state classifier-bakeoff \
  --fixtures apps/bitrouter/tests/fixtures/workflow_state \
  --output scorecard-bakeoff.json
```

An external runner reads the source fixtures, produces one prediction per
manifest case, and binds every record to the manifest's dataset and input
projection digests. A learned submission also declares model-artifact, feature,
training-split, and calibration provenance. Calibrated categorical heads contain
the complete canonical label list in canonical order and sum to exactly one
million ppm. Capability scores are multilabel and therefore do not sum to one.

Evaluate the candidate without changing daemon or policy state:

```bash
bro workflow-state classifier-bakeoff \
  --fixtures apps/bitrouter/tests/fixtures/workflow_state \
  --submission candidate.json \
  --output candidate-bakeoff.json
```

The result artifact intentionally contains commitments and labels, not prompt
text. Explicit abstention preserves the candidate's predicted labels while
removing the case from accepted-risk calculations; it is never rewritten as an
`unknown` class. Missing probability or OOD metrics stay absent rather than
being interpreted as zero.

Report and artifact schema v2 correct the ECE scale and represent a slice with
no accepted predictions as `accepted_error_risk_ppm: null`. ECE is in
`0..1_000_000` ppm; the unnormalized multiclass Brier score is in
`0..2_000_000` ppm. Recompute v1 reports from the source predictions rather than
interpreting a zero ECE as evidence of calibration.

The v2 field `classification_surrogate_loss` replaces `decision_weighted_loss`.
Its fixed task/role/progress/risk penalties (4/3/2/8, and 17 for abstention)
measure label errors only. They do not run a policy, price the chosen target,
or distinguish risk underestimation from overestimation. Actual decision loss
requires replaying a frozen policy and its guards, including the fallback for
abstention, and evaluating the resulting model/effort and task outcomes.

The scorecard's unknown-task abstention is a diagnostic convention of this
bake-off. Live policies can still route unknown task families using role and
risk; bake-off coverage is not the live routing success rate.

## Promotion gates

A candidate may advance to a larger shadow trial only when all of the following
are specified before viewing the sealed results:

- macro-F1 and critical-class recall improve over the scorecard and simpler
  learned candidates;
- multilingual, short-input, phase, harness, and OOD slices meet minimum sample
  counts and regression limits;
- ECE, multiclass Brier score, and risk-coverage behavior meet declared limits,
  with minimum accepted sample counts; an unobserved risk cannot pass a gate;
- guarded-risk false negatives do not regress; a learned risk score may only
  add evidence and cannot lower deterministic risk;
- median/p95 CPU latency, peak memory, artifact size, and cold-start cost fit
  the deployment budget;
- classification surrogate loss and coverage both pass, so abstaining on every
  case cannot win the bake-off; claims about routing improvement additionally
  require policy replay and a task-outcome experiment;
- results reproduce from committed inputs and artifacts on a second machine.

Only a later phase may use shadow evidence in routing, and that requires a
separate signed policy change, rollback plan, and online measurement design.

For that experiment, define the task/session randomization unit and the
quality difference from control before choosing a stopping rule. Non-inferiority
requires a lower confidence bound on `quality(candidate) - quality(control)`
above the allowed negative margin; comparing the two arms' lower bounds does
not establish it. Request rows from one assignment are not independent trials,
and pre-guard logging propensities are not effective-model propensities after
guards or continuation pins. Selective evaluators should retain a random audit
sample and record their sampling probabilities and missing-label reasons.
