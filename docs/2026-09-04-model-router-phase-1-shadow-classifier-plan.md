# Model Router Phase 1 Shadow Classifier Plan

**Goal:** Introduce an auditable Route Context V3 shadow-classifier contract
and a deterministic offline bake-off. Candidate classifiers may emit semantic
evidence and uncertainty, but cannot affect live routing in this phase.

**Architecture:** Existing extraction, scorecard, policy, and safety guards stay
authoritative. A research-only exchange exports frozen fixture identities and
labels, admits externally produced predictions under strict provenance and
probability rules, and evaluates them with integer/rational metrics. Task,
role/progress, and risk are separate heads. Risk labels remain rule-owned even
when a candidate reports shadow risk probabilities.

**Candidate order:** Current deterministic scorecard; multilingual embedding
plus class prototypes; frozen multilingual encoder plus a linear head; frozen
encoder plus a two-layer MLP; then a distilled static/tiny encoder. Candidates
advance only after held-out, multilingual, short-input, and OOD evidence. SetFit
and Model2Vec are implementation candidates, not embedded dependencies or
endorsed models in this phase.

## Invariants

- Shadow output is evidence only. It has no reference from the live policy
  table, route selector, guards, or model target.
- Task/capability, role/progress, and risk are separately represented.
- Calibrated class probabilities use integer ppm, are canonically ordered, and
  total exactly `1_000_000` per head. Heuristic margins cannot be serialized as
  calibrated probabilities.
- Every prediction binds dataset, input projection, model artifact, feature,
  and optional calibration digests.
- OOD score and abstention are explicit. Abstention is evaluated as coverage,
  never silently coerced into an `unknown` prediction.
- Candidate files require exactly one prediction for every frozen fixture and
  reject unknown, duplicate, missing, reordered, or cross-dataset records.
- Model size, peak memory, and latency are reported measurements with bounded
  integer units; missing measurements remain visibly absent.
- Bake-off results are deterministic and use integer counts or fixed-point ppm.
- The frozen research fixtures are evaluation inputs, not an in-repository
  training set. No accuracy claim may be called held-out without a disjoint
  split commitment.

## Task 1: Define Route Context V3 shadow evidence

**Files:** new `workflow_state/shadow_classifier.rs`, `workflow_state/mod.rs`.

- [x] Add task-family, role, progress, and risk distributions with canonical
  labels and strict ppm validation.
- [x] Add predictor provenance, input projection digest, OOD score,
  abstention/reason, and bounded exemplar evidence.
- [x] Reject malformed versions, identifiers, digests, probability totals,
  predicted-label mismatches, fabricated calibration, and inconsistent
  abstention.
- [x] Prove by dependency audit that the contract has no live-routing consumer.

## Task 2: Freeze an offline prediction exchange

**Files:** `workflow_state/classifier_baseline.rs`, fixtures, CLI wiring.

- [x] Export a canonical dataset manifest and ordered evaluation cases without
  writing private prompt text to the result artifact.
- [x] Load and validate candidate prediction JSON with full fixture coverage,
  stable dataset/input commitments, and declared split provenance.
- [x] Add a research-only CLI that emits the manifest or evaluates a candidate
  file without changing configuration or daemon state.

## Task 3: Implement deterministic bake-off metrics

**Files:** new `workflow_state/classifier_bakeoff.rs` and tests.

- [x] Report task/role/progress/risk exact counts and macro-F1 ppm.
- [x] Report per-slice accepted coverage, accepted error risk, OOD detection,
  Brier score, and fixed-bin ECE only when calibrated probabilities exist.
- [x] Report decision-weighted loss with explicit, versioned weights and no
  hidden evaluator call.
- [x] Preserve hardware measurements as provenance; do not compare absent
  latency, memory, or size values as zero.

## Task 4: Establish honest baselines and candidate protocol

**Files:** frozen fixtures and documentation.

- [x] Emit the current scorecard as an uncalibrated baseline with no fabricated
  probability or OOD score.
- [x] Add valid synthetic candidate fixtures only for admission/metric tests;
  label them as test vectors, never research results.
- [x] Document the external training/evaluation split: collect labels, split by
  task/session before fitting, calibrate on a disjoint set, and evaluate once on
  a sealed test set.
- [x] Document promotion gates for quality, calibration/selective risk,
  multilingual/OOD slices, and CPU/memory/latency budgets.

## Task 5: Verification and handoff

- [x] Run focused RED/GREEN tests, formatting, Clippy, all-feature workspace
  tests, doctests, and dist-helper checks.
- [x] Obtain independent review and resolve every finding.
- [x] Update the external research report without reading its `archive/`
  directory.
- [ ] Push a PR, wait for required CI, and squash-merge it to `main`.

## Acceptance criteria

1. A malformed or incomplete candidate prediction set fails closed.
2. Calibrated distributions cannot omit, duplicate, reorder, or mis-sum labels.
3. Heuristic confidence cannot masquerade as calibrated probability.
4. Abstention and OOD are measurable independently from `unknown` classes.
5. Candidate provenance binds all artifacts needed to reproduce inference.
6. Reports show macro quality, calibration, selective risk, slices, resource
   measurements, and decision-weighted loss without floating-point wire values.
7. The current scorecard baseline remains reproducible and visibly
   uncalibrated.
8. No Phase 1 type or result can change a live route.
9. Documentation distinguishes test vectors, development results, held-out
   evidence, and production promotion.
10. Full repository checks and independent review pass.

## Non-goals

- Shipping model weights, an inference runtime, or a Python training stack.
- Selecting a winning encoder from ten research fixtures.
- Replacing deterministic risk guards or signed policy locks.
- Online learning, bandit promotion, or policy evolution; those belong to later
  phases after shadow evidence and measurement are trustworthy.
