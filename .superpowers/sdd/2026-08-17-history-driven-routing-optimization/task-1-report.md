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
