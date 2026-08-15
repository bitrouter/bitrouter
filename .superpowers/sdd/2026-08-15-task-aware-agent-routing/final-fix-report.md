# Task-Aware Agent Routing Final Fix Report

## Status

Complete. All seven Important findings and all four Minor findings from
`final-review-findings.md` are implemented. Focused regressions are green, the
full required gate set passes, and an independent final diff review found no
remaining actionable issue.

Implementation commit:

- `2a32bde2d13369619ee7e03904ac8cc5e2437ddb` —
  `fix: complete task-aware routing contract`

## Claim Verification and RED/GREEN Evidence

Each review claim was traced through the real runtime path before production
code was edited. The main paths were:

- routing: `OnlineWorkflowState` -> `PolicyTableRouter` -> pending eval / JSONL;
- ordinary settlement: pending decision -> `EvalDecisionRef` -> frozen eval
  snapshot -> policy compiler;
- guarded settlement: route-intent trajectory evidence -> trajectory eval
  decoder -> `EvalDecisionRef` -> compiler;
- optimization: JSONL decision + eval subject exact join -> route observation ->
  target selection -> experiment lock;
- prediction: protocol extractor -> normalized history -> instruction and task
  classifiers -> canonical v1/v2 route projection;
- lock admission: route-version detection -> exact predictor-contract check.

### Important 1 — sparse-fallback evidence attribution

Verified defect: routing retained a v2 primary projection in memory but pending
and durable eval records retained only the matched v1 key. The compiler grouped
by that v1 key, and baseline lookup used the v1 certificate's comparison
baseline instead of the v1 route tier actually inherited.

Focused test:

```text
cargo test -p bitrouter --lib sparse_v2_settlement_compiles_against_inherited_v1_tier -- --nocapture
```

- RED: the real router -> settlement -> snapshot -> compiler test first exposed
  the absent primary/fallback fields at compile time; after introducing only
  the structural fields, its behavior assertions showed v1 attribution and the
  wrong comparison baseline.
- GREEN: an unlisted
  `agent_route/v2|code:generation|implement|normal` primary matches
  `agent_route/v1|implement|normal`, records the v1 route's `balanced` tier,
  survives settlement, groups evidence under the v2 key, retains the matched v1
  key, and emits a v2 certificate with `balanced` as baseline.

Final-audit regression:

```text
cargo test -p bitrouter --lib task_aware_policy_v2_override_wins_before_observed_state -- --nocapture
```

- RED: an exact v2 match was incorrectly labeled with an available v1 tier.
- GREEN: the fallback tier is populated only when the matched key is the exact
  compatibility v1 key; exact v2 matches retain `None`.

### Important 2 — predictive optimization support

Verified defect: optimizer selection and experiment construction parsed only
observed `agent_trace` keys. Predictive v1/v2 observations were skipped, an
unlisted v2 target used the policy default rather than its v1 compatibility
cell, and predictive orchestration lacked opening semantics.

Focused tests:

```text
cargo test -p bitrouter --lib optimizer_accepts_task_aware_keys_and_inherits_v1_baseline -- --nocapture
cargo test -p bitrouter --lib optimizer_treats_predictive_orchestrate_as_opening -- --nocapture
```

- RED: the first test failed because no compiler-owned strong route could be
  selected from the canonical v2 observation.
- GREEN: `CanonicalPolicyProjection` accepts observed, predictive v1, and
  task-aware v2 keys; selection targets the persisted primary projection;
  predictive `orchestrate` obeys `explore_opening`; and an absent v2 route uses
  exact v2 -> compatibility v1 -> default baseline precedence.

### Important 3 — v1 lock upgrade compatibility

Verified defect: any predictive route required the current compiled predictor
contract, so the one previously shipped v1-only contract was rejected after
upgrade.

Focused test:

```text
cargo test -p bitrouter --lib prior_predictor_contract_is_admitted_only_for_v1_routes -- --nocapture
```

- RED: the unchanged prior descriptor with digest
  `sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec`
  was rejected against the current contract.
- GREEN: that exact algorithm/version/digest/confidence/calibration tuple is
  accepted only when a policy has v1 routes and no v2 routes. The same tuple
  with v2, an arbitrary digest, or any mutated algorithm, version, confidence
  kind, or calibration digest is rejected. Current mixed v1/v2 locks continue
  to require the current exact contract.

### Important 4 — protocol-independent task pivots

Verified defect: Terminus alone supplied normalized plain-text action history,
causing instruction and task classification to truncate at the first assistant
turn. Codex, Claude, and generic adapters selected the latest user text.

Focused test:

```text
cargo test -p bitrouter --test agent_trace_generalization task_aware_cross_harness_routes_equivalent_requests_identically -- --exact --nocapture
```

- RED: the real HTTP matrix classified the Terminus pivot as the original
  review/unknown route while the other three harnesses classified the later
  regression fix.
- GREEN: Codex Responses, Claude Messages, Terminus Chat Completions, and generic
  Chat Completions all select the latest causal user pivot, classify
  `code:debugging` / `implement`, and use the same v2 primary and v1 matched
  route. Structured tool-result content is excluded, Terminus plain action
  results are skipped, and bounded/incomplete histories still fail closed to
  unknown.

Final-audit regression:

```text
cargo test -p bitrouter --lib normalized_history_skips_only_actual_action_results -- --nocapture
```

- RED: treating every assistant message as an action boundary could hide a user
  pivot after an ordinary assistant response.
- GREEN: only an assistant message that parses as a real Terminus JSON/XML
  action causes the immediately following plain user result to be skipped.

### Important 5 — review intent precedence

Verified defect: `Review the bug fix.` scored debugging 32 and review 28.

Focused test:

```text
cargo test -p bitrouter --lib task_family_review_intent_precedes_debugging_subject_terms -- --nocapture
```

- RED: the exact literal returned `CodeDebugging` instead of `CodeReview`.
- GREEN: explicit review/audit intent dominates debugging subject words only
  when a serialized code-subject signal is present. `Fix the bug.` and
  `Repair a regression.` remain debugging; non-code `Review the schedule.`
  remains unknown.

### Important 6 — shell pipeline is not agent workflow

Verified defect: `execute` plus `pipeline` met the workflow family threshold
without any agent-orchestration meaning.

Focused test:

```text
cargo test -p bitrouter --lib task_family_workflow_requires_an_orchestration_anchor -- --nocapture
```

- RED: `Execute the shell pipeline and report its output.` returned
  `AgentWorkflowExecution`.
- GREEN: shell, file, and tool-dispatch pipelines remain unknown without a
  serialized orchestration anchor; explicit agent orchestration and workflow
  handoff examples still classify as workflow execution.

### Important 7 — classifier digest completeness

Verified defect: debugging intent/failure bonuses and other task-classifier
control groups were hard-coded outside `PredictorBehaviorV1`, allowing routing
changes without a predictor digest change.

Focused test:

```text
cargo test -p bitrouter --lib predictor_contract_digest_covers_every_task_classifier_component -- --nocapture
```

- RED: the regression test could not name the missing serialized task
  classifier groups.
- GREEN: the serialized behavior now contains the task-classifier algorithm
  version, base terms, generic/specific modifier families, intent terms,
  failure terms, precedence terms, workflow anchors, code-subject terms,
  scorecard, and tie order. Mutating each group changes the digest.

The new compiled predictor digest is:

```text
sha256:aa204ef3be199ffa8911e380e3dec214fb1070b28b113fa3c413e38703314ec6
```

The deterministically regenerated template compiler-config digest is:

```text
sha256:c1f54394ab38097092de016c85df0914bc362176e083943d511cb75d139e24a9
```

## Minor Findings

All four Minor items were completed; none was deferred.

1. Task-family reason codes now propagate through ordinary eval evidence and
   trajectory settlement. They are whitelist-filtered, sorted, deduplicated,
   capped at eight entries, and capped at 128 joined bytes. Invalid values such
   as `customer_secret` are discarded.
2. The real four-harness matrix includes a confident unlisted-v2 case and
   asserts distinct primary v2 and matched v1 keys. The unknown test separately
   proves an explicit v1 `economy` route wins before a `strong` default.
3. The template contract now asserts the exact tier-key set
   `{economy, balanced, strong}`.
4. Every cross-harness route assertion includes the scenario and harness case
   name in its diagnostic.

Additional bound tests observed RED and then GREEN for an eight-code joined
categorical value exceeding 128 bytes and for overlong primary/fallback fields
in imported eval subjects.

## Schema and Compatibility Design

- `PolicyDecisionRecord` adds optional `route_projection` and
  `predictive_v1_fallback_tier` fields with serde defaults and omission when
  absent.
- `EvalDecisionRef` adds the same optional/defaulted fields. Legacy JSON without
  them decodes to `None` and serializes back to the same semantic JSON shape.
- `RouteObservation` adds an optional/defaulted primary projection, so old
  optimizer artifacts continue to use `request_key`.
- `PendingEvalDecision` is process-local and carries required primary/matched
  identity, fallback tier, and bounded task reason codes to settlement.
- Guarded trajectory events persist primary projection, matched request key,
  and optional fallback tier as bounded categorical evidence. Old events omit
  the new keys and decode to `None`.
- New durable eval identifiers are validated with the existing 512-byte,
  non-empty, no-control-character bound. Reason-code evidence contains only
  fixed categorical codes; no prompt, action output, identity, or secret text
  is retained.
- Compiler evidence is keyed by primary projection, while matched request keys
  are retained in deterministic evidence provenance. The actual predictive-v1
  fallback tier takes precedence over the generic certificate comparison
  baseline only when v1 really matched.
- Canonical optimizer parsing supports all three policy projections. A v2
  compatibility key is derived by the typed
  `TaskAwarePredictiveRouteProjection::compatibility_projection_v1` method,
  avoiding string surgery.
- Existing v1 routing precedence and role/risk keys remain unchanged. The only
  legacy lock relaxation is the exact prior shipped v1 descriptor, and it is
  never accepted for a policy containing v2 routes.

## Files Changed

Routing, contracts, and compilation:

- `apps/bitrouter/src/policy_table_router.rs`
- `apps/bitrouter/src/policy_lock.rs`
- `apps/bitrouter/src/policy_compile.rs`
- `apps/bitrouter/src/workflow_state/decision.rs`
- `apps/bitrouter/src/workflow_state/predictive.rs`
- `apps/bitrouter/src/workflow_state/extractors/terminus_2.rs`

Eval and optimization:

- `apps/bitrouter/src/eval/compiler.rs`
- `apps/bitrouter/src/eval/settlement.rs`
- `apps/bitrouter/src/eval/store.rs`
- `apps/bitrouter/src/eval/types.rs`
- `apps/bitrouter/src/optimization/orchestrator.rs`
- `apps/bitrouter/src/optimization/runner.rs`

Trajectory and observation plumbing:

- `apps/bitrouter/src/trajectory/evaluation.rs`
- `apps/bitrouter/src/trajectory/settlement.rs`
- `apps/bitrouter/src/trajectory/store.rs`
- `apps/bitrouter/src/output/reports/trajectory.rs`
- `apps/bitrouter/src/workflow_state/response_observer.rs`
- `apps/bitrouter/src/workflow_state/reward_feedback.rs`

Integration fixtures and compatibility tests:

- `apps/bitrouter/tests/agent_trace_generalization.rs`
- `apps/bitrouter/tests/policy_eval_control_plane.rs`
- `apps/bitrouter/tests/smithers_reward_loop.rs`
- `apps/bitrouter/tests/workflow_state_replay.rs`
- `templates/auto-router/policy-lock.yaml`

This standalone report is
`.superpowers/sdd/2026-08-15-task-aware-agent-routing/final-fix-report.md`.

## Full Gate Output Summaries

All commands were run from the task worktree after the final production edits.

```text
cargo test --all-features
```

Exit 0. The main BitRouter library suite passed 1,104/1,104 tests; the real
task-aware HTTP matrix passed 10/10 integration tests; all remaining workspace
unit, integration, and doctest suites completed with zero failures. Tests
marked ignored by their existing suite configuration remained ignored.

```text
cargo clippy --all-features -- -D warnings
```

Exit 0. The all-feature workspace completed with no warnings.

```text
cargo fmt -- --check
```

Exit 0 with no output.

```text
git diff --check
```

Exit 0 with no whitespace errors.

## Self-Review

- Re-read every review finding against the final diff and traced primary versus
  matched identity through both ordinary and guarded settlement.
- Added an exact-v2 regression after self-review found fallback metadata could
  be populated merely because a compatibility route existed.
- Narrowed the Terminus causal boundary to parsed actions after reviewing the
  mixed ordinary-assistant/action case.
- Added byte and identifier bounds after checking trajectory's categorical
  evidence validator rather than relying only on an entry-count bound.
- Verified old serialized decision, eval, route-observation, and trajectory
  shapes default the new fields and remain readable.
- Verified the prior lock exception compares the whole known descriptor and
  cannot admit v2 or a partially matching legacy contract.
- An independent read-only final diff review checked all seven Important and
  four Minor paths, serde compatibility, privacy bounds, canonical parsing, and
  existing v1 behavior; it reported no actionable findings.
- No broad lint suppressions, public compatibility reexports, or production
  panic shortcuts were introduced.

## Residual Concerns

- Terminus plain-text transport cannot intrinsically distinguish an immediate
  user action result from a new instruction. The implementation preserves the
  shipped Terminus contract by treating the first plain user turn after a
  parsed action as its result; later causal user pivots are selected, while
  incomplete histories remain unknown.
- The prior predictor admission is intentionally an exact one-entry allowlist.
  Any future shipped legacy predictor contract requires an explicit reviewed
  addition rather than a range or partial-match rule.
- Task-family classification remains a deterministic heuristic. Its complete
  causal configuration is now signed by the predictor digest, so any future
  classifier behavior change must update the contract and regenerated template.

There are no known remaining correctness blockers and no deferred Minor item.
