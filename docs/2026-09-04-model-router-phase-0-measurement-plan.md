# Model Router Phase 0 Measurement Plan

**Goal:** Freeze a replayable measurement contract before changing the semantic
classifier. Every routed decision must expose the declared semantic action set,
the pre-guard logging action, and its probability. Benchmark bundles must emit
content-blind controls whose tier shares exactly match the observed router.

**Architecture:** The active policy remains authoritative and unchanged. The
router attaches an optional, versioned measurement object to diagnostic and
Eval decision evidence. A separate offline evaluator consumes only redacted
records, groups compatible candidate sets, and deterministically builds
always-tier and share-matched assignments. Research fixtures freeze the current
predictor on English, Chinese, mixed-language, short, and out-of-distribution
slices. No Phase 0 measurement may influence live routing.

**Tech stack:** Rust, Serde, SHA-256 domain-separated commitments, the existing
workflow-state replay/bundle path, Cargo nextest/test, Clippy, rustfmt.

## Invariants

- Product code and comments are English and contain no private-service or
  private-workspace references.
- The measurement action is the semantic policy action before tool, progress,
  or continuation guards. The effective selected tier remains a separate field.
- Candidate probabilities are integer ppm, sum to `1_000_000`, and bind the
  logged action to one declared tier/model/effort target.
- Deterministic policies log probability `1_000_000` for their action.
- Active experiments log champion/challenger probabilities from signed state.
  Missing stable assignment identity fails safe to deterministic champion.
- Candidate-set and dataset commitments use explicit domains and canonical
  ordering. Raw prompt text and raw request identity never enter baselines.
- A share-matched baseline is content blind, deterministic, and preserves exact
  selected-tier counts within each candidate-set group.
- Baselines are measurement controls only, not counterfactual quality claims.
- Existing JSON without the optional measurement object remains readable.

## Task 1: Define the route measurement wire contract

**Files:** `eval/types.rs`, `workflow_state/decision.rs`, and all decision
construction sites.

- [x] Add RED tests for serialization, canonical ordering, ppm totals,
  action/candidate consistency, bounds, digest validation, and legacy omission.
- [x] Add `RouteActionCandidate` and `RouteDecisionMeasurement` to Eval schema
  v1 as an optional backward-compatible decision field.
- [x] Carry the same object in policy-decision JSONL.
- [x] Reject malformed or forged probability evidence at Eval admission.

## Task 2: Populate measurement from one policy snapshot

**Files:** `policy_table_router.rs`, `eval/settlement.rs`.

- [x] Add RED tests for deterministic, challenger, control, missing stable
  identity, tool-floor, progress-guard, and continuation-pin decisions.
- [x] Derive a sorted candidate set from the same immutable table snapshot used
  for the route decision.
- [x] Record pre-guard action/probability and retain effective route separately.
- [x] Carry measurement through settlement into immutable Eval subjects.

## Task 3: Generate deterministic benchmark controls

**Files:** new `workflow_state/measurement.rs`, `workflow_state/mod.rs`,
`workflow_state/archive.rs`, and replay integration tests.

- [x] Add RED tests for exact share preservation, deterministic assignment,
  candidate-set isolation, legacy exclusion, redaction, and stable digest.
- [x] Build one always-tier baseline per target and one share-matched baseline
  per candidate-set group.
- [x] Embed the report in `run-artifact.json` and write
  `routing-baselines.json` in benchmark bundles.
- [x] Report legacy exclusions rather than failing in-memory development use.

## Task 4: Freeze multilingual and OOD replay slices

**Files:** `workflow_state/fixture.rs`, fixtures under
`tests/fixtures/workflow_state/classifier_baseline/`, and replay tests.

- [x] Add optional diagnostic research slice labels to fixtures.
- [x] Add hand-labeled English, Chinese, mixed-language, short, and OOD cases
  covering opening, post-read, post-mutation, failure, and completion.
- [x] Freeze a literal dataset digest and slice counts.
- [x] Report current scorecard behavior without changing the predictor.

## Task 5: Documentation and verification

**Files:** `docs/CLI.md`, `skills/bitrouter/references/metering.md`.

- [x] Document pre-guard action semantics and baseline limitations.
- [x] Run focused RED/GREEN suites.
- [x] Run format, Clippy, all-feature test, and dist-helper checks.
- [x] Obtain independent review and resolve every finding.

## Acceptance criteria

1. Deterministic decisions record all declared targets and a one-million-ppm
   action.
2. Assigned experiments record both arm probabilities and actual action
   probability.
3. Missing experiment identity records deterministic champion, not fabricated
   randomized evidence.
4. Guards do not rewrite the pre-guard action; effective fields show overrides.
5. Eval rejects invalid sums, duplicates, ordering, action mismatch, digest, and
   bounds.
6. Old Eval and decision JSON remains readable without synthetic measurement.
7. Bundles contain deterministic always-tier and share-matched controls with
   exact per-group counts and no raw request IDs.
8. Candidate-set groups cannot exchange unsupported tiers.
9. Research fixtures have a frozen digest and all declared slices.
10. Complete repository checks and independent review pass.

## Non-goals

- No new classifier or live routing behavior.
- No quality claim for an unexecuted model.
- No off-policy estimator or promotion-rule change.
- No provider-registry digest in this semantic candidate set; provider routing
  is a later physical decision with separate provenance.
- No model-weight digest claim for remote APIs. The exact configured model ID is
  recorded; execution-layer version provenance remains separate.
