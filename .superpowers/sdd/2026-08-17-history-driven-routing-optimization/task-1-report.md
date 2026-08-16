# Task 1 Report: Signed Experiment Metadata and Stable Runtime Assignment

## Implemented behavior

- Added bounded signed exploration state in `optimization::exploration`, including deterministic task/episode assignment with a redacted SHA-256 assignment identity digest.
- Added optional, backward-compatible experiment references to Eval decisions and validation for both digests and propensity bounds.
- Added optional signed policy optimization state, validation of target keys, tier references, exposure/gate bounds, evaluator digest, bounded rejections, and deterministic YAML omission for empty state.
- Attached active named-policy exploration at runtime, applied assignment before tool/progress safeguards, and retained signed arm metadata when safeguards clamp the selected tier upward.
- Propagated experiment evidence through pending request settlement and guarded trajectory route evidence. Trajectory decoding accepts legacy absence, requires an all-or-none experiment evidence group, and validates its categorical, structural, and digest fields.

## Files changed

- Added `apps/bitrouter/src/optimization/exploration.rs`.
- Updated optimization, Eval, policy lock/router, and trajectory modules, plus existing test fixtures that construct `EvalDecisionRef` or `PendingEvalDecision`.

## TDD evidence

### RED

1. `cargo test -p bitrouter eval::types::tests --all-features`
   - Failed as expected with missing `EvalExperimentRef`, `ExperimentArm`, `ExperimentAssignmentUnit`, and `EvalDecisionRef::experiment` symbols.
2. `cargo test -p bitrouter policy_lock::tests --all-features`
   - Failed as expected with missing `PolicyOptimizationState`, `RouteExploration`, `OptimizationGate`, and `PolicyDefinition::optimization`.
3. While adding deterministic-YAML coverage, the first fixture run failed because a v3 lock with a route also requires a v2 certificate. The failure identified an invalid test fixture rather than production behavior; the fixture was reduced to one tier without routes, which is sufficient to test omission.

### GREEN

- `cargo test -p bitrouter eval::types::tests --all-features` — passed.
- `cargo test -p bitrouter policy_lock::tests --all-features` — 45 passed.
- `cargo test -p bitrouter optimization::exploration::tests --all-features` — 2 passed.
- `cargo test -p bitrouter policy_table_router::tests --all-features` — 47 passed.
- `cargo test -p bitrouter eval::settlement::tests --all-features` — 4 passed.
- `cargo test -p bitrouter trajectory::evaluation::tests --all-features` — 8 passed.
- `cargo fmt -- --check` — passed.
- `cargo clippy --all-features --quiet` — passed.
- `cargo test --all-features` — passed; main library suite reported 1,079 passed, 0 failed, and all workspace/integration/doc-test suites completed successfully.

## Self-review

- Verified stable assignment uses only `(benchmark_run_id, trial_id)` for task assignment or `parent_session_id` for episode assignment; request ids and context fingerprints are never fallbacks.
- Verified target comparison uses the resolved canonical request key, non-target routes stay champion, and guardrail clamping retains the original experiment reference.
- Verified legacy Eval JSON and legacy trajectory route evidence remain valid, while partial trajectory experiment evidence is rejected.
- Verified no public re-exports were added and the previous optimizer/CLI modules remain intact.

## Findings

No unresolved findings.

## Review-fix chronology (after `5115c7e4`)

The following evidence belongs to the review-fix commit, not the original Task 1 chronology.

### Review-fix behavior

- Missing stable assignment identity now explicitly resolves to the signed champion tier, and signed exploration validation requires the target route to map to that champion.
- Validation tests now independently cover zero exposure, a missing tier, zero gate budgets, and a 257-entry rejection ledger.
- Added a progress-guard clamp regression and expanded settlement/trajectory assertions to verify all five experiment fields.
- The guarded-route producer now has an end-to-end test through operational trajectory evaluation; decoder tests also cover partial and malformed experiment evidence.

### Review-fix RED evidence

1. `cargo test -p bitrouter policy_table_router::tests::exploration_without_stable_identity_uses_signed_champion_control --all-features`
   - Failed before the fix: the selected tier was `economy` from the route table instead of signed control `strong`.
2. `cargo test -p bitrouter policy_lock::tests::optimization_target_must_match_the_signed_champion_route --all-features`
   - Failed before the fix because a target route mapped to `economy` while the signed champion was `strong` was accepted.
3. Existing propagation behavior predated the review-fix tests, so new tests initially passed. To establish test sensitivity without inventing chronology, I made two temporary local mutations and restored each immediately:
   - Replaced settlement propagation with `experiment: None`; `cargo test -p bitrouter eval::settlement::tests::settlement_creates_a_redacted_request_subject --all-features` failed with `None` instead of `Some(Challenger)`.
   - Removed the guarded-route producer's assignment copy; `cargo test -p bitrouter trajectory::store::tests::guarded_route_producer_emits_complete_experiment_evidence --all-features` failed because `route.experiment_id` was absent.

### Review-fix GREEN evidence

- `cargo test -p bitrouter policy_table_router::tests --all-features` — 49 passed.
- `cargo test -p bitrouter policy_lock::tests --all-features` — 48 passed.
- `cargo test -p bitrouter eval::settlement::tests --all-features` — 4 passed.
- `cargo test -p bitrouter trajectory::store::tests::guarded_route_producer_emits_complete_experiment_evidence --all-features` — 1 passed.
- `cargo test -p bitrouter trajectory::evaluation::tests --all-features` — 8 passed.
- `cargo fmt -- --check` and `cargo clippy --all-features --quiet` — passed after the review fixes.
- `cargo test --all-features` — passed; main library suite reported 1,085 passed, 0 failed, and the workspace/integration/doc-test suites completed successfully.

### Review-fix files changed

- `apps/bitrouter/src/policy_table_router.rs`
- `apps/bitrouter/src/policy_lock.rs`
- `apps/bitrouter/src/eval/settlement.rs`
- `apps/bitrouter/src/trajectory/store.rs`
- `apps/bitrouter/src/trajectory/evaluation.rs`
- `.superpowers/sdd/2026-08-17-history-driven-routing-optimization/task-1-report.md`

## Gate-boundary test follow-up (after `731887be`)

This test-only follow-up adds independently mutation-sensitive coverage for the two remaining zero-valued gate fields: `maximum_challenger_tasks` and `minimum_pass_rate_ppm`. Production validation behavior was already present and was not changed.

### Follow-up RED evidence

- The new tests initially passed against existing behavior: `cargo test -p bitrouter optimization_rejects_zero --all-features` reported 2 passed. To verify sensitivity honestly, I made temporary local mutations and restored each immediately.
- Removing only the `maximum_challenger_tasks == 0` arm of the positive-budget validation initially still allowed an error from the later ordering rule. I therefore tightened the new test to require the intended `sample budgets must be positive` rejection. With that temporary mutation, `cargo test -p bitrouter policy_lock::tests::optimization_rejects_zero_maximum_challenger_tasks --all-features` failed at the expected error-message assertion.
- Temporarily widening the pass-rate range to allow zero caused `cargo test -p bitrouter policy_lock::tests::optimization_rejects_zero_minimum_pass_rate --all-features` to fail because `validate_route_exploration` returned `Ok(())` instead of an error.

### Follow-up GREEN evidence

- `cargo test -p bitrouter policy_lock::tests --all-features` — 50 passed.
- `cargo fmt -- --check` — passed.
- `cargo clippy --all-features --quiet` — passed.

### Follow-up files changed

- `apps/bitrouter/src/policy_lock.rs` (tests only)
- `.superpowers/sdd/2026-08-17-history-driven-routing-optimization/task-1-report.md`
