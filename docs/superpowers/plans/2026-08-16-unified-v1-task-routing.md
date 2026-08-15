# Unified v1 Task-Aware Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace dual predictive route protocols and their compatibility layer with one breaking four-segment `agent_route/v1` contract, then update all policy, evidence, optimizer, template, and documentation surfaces.

**Architecture:** One `PredictiveRouteProjection` owns task family, role, and risk. Named auto routing resolves an exact four-segment v1 key, then the same role/risk under task family `unknown`, then the policy default. Compatibility-only types, fields, contract admissions, and observed-route policy fallbacks are deleted; primary and matched route keys retain generic attribution.

**Tech Stack:** Rust, Serde, YAML/JSON policy locks, Tokio HTTP integration tests, Cargo workspace checks.

## Global Constraints

- The only predictive policy-key shape is `agent_route/v1|<task-family>|<role>|<risk>`.
- Old three-segment v1 and every agent-route v2 key are rejected; no migration path is retained.
- `agent_trace/v2` remains telemetry only and never participates in named auto-policy lookup.
- Predictor contract version stays `1` and only the current exact descriptor is admitted.
- No `#[allow(...)]`, production panic path, public re-export, or unrelated refactor.
- Update `skills/bitrouter/` and `templates/auto-router/` in the same change as the routing contract.

---

### Task 1: Collapse predictive projections into one v1 type

**Files:**
- Modify: `apps/bitrouter/src/workflow_state/predictive.rs`
- Modify: `apps/bitrouter/src/workflow_state/online.rs`
- Modify: `apps/bitrouter/tests/agent_trace_generalization.rs`
- Modify: `apps/bitrouter/tests/workflow_state_replay.rs`

**Interfaces:**
- Produces: `PredictiveRouteProjection::new(task_family: TaskFamily, next_step_role: NextStepRole, risk: RouteRisk) -> Self`.
- Produces: `PredictiveRouteProjection::key() -> String` using the four-segment v1 shape.
- Produces: `PredictiveRouteProjection::unknown_baseline() -> Self` preserving role/risk and replacing task family with `TaskFamily::Unknown`.
- Removes: `TaskAwarePredictiveRouteProjection` and all compatibility projection accessors.

- [ ] **Step 1: Write parser and online-state RED tests**

```rust
#[test]
fn predictive_projection_has_one_v1_task_aware_shape() {
    let route = PredictiveRouteProjection::new(
        TaskFamily::CodeReview,
        NextStepRole::Verify,
        RouteRisk::Normal,
    );
    assert_eq!(route.key(), "agent_route/v1|code:review|verify|normal");
    assert_eq!(
        route.unknown_baseline().key(),
        "agent_route/v1|unknown|verify|normal"
    );
    assert_eq!(PredictiveRouteProjection::parse_key(&route.key()), Some(route));
}

#[test]
fn predictive_projection_rejects_retired_shapes() {
    assert!(PredictiveRouteProjection::parse_key("agent_route/v1|verify|normal").is_none());
    assert!(PredictiveRouteProjection::parse_key(
        "agent_route/v2|code:review|verify|normal"
    ).is_none());
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test -p bitrouter --all-features predictive_projection_has_one_v1_task_aware_shape`

Expected: compilation or assertion failure because `PredictiveRouteProjection::new` lacks task family and still produces the three-segment key.

- [ ] **Step 3: Implement the single projection and online state**

```rust
pub struct PredictiveRouteProjection {
    pub task_family: TaskFamily,
    pub next_step_role: NextStepRole,
    pub risk: RouteRisk,
}

pub const fn unknown_baseline(&self) -> Self {
    Self::new(TaskFamily::Unknown, self.next_step_role, self.risk)
}
```

Make `OnlineWorkflowState::routing_key()` return this projection for every prediction, including `TaskFamily::Unknown`. Add `baseline_routing_key()` for its unknown-family projection. Remove predictive/observed compatibility key storage and accessors from the named-policy state.

- [ ] **Step 4: Update replay and cross-harness expectations**

Convert predictive route expectations from old three-segment v1 or v2 to four-segment v1. Keep observed `agent_trace/v2` assertions only where they verify telemetry rather than static policy selection.

- [ ] **Step 5: Run focused workflow suites and verify GREEN**

Run: `cargo test -p bitrouter --all-features workflow_state::predictive`

Run: `cargo test -p bitrouter --all-features workflow_state::online`

Run: `cargo test -p bitrouter --test agent_trace_generalization --all-features`

Expected: all selected tests pass with no agent-route v2 output.

- [ ] **Step 6: Commit**

```bash
git add apps/bitrouter/src/workflow_state/predictive.rs apps/bitrouter/src/workflow_state/online.rs apps/bitrouter/tests/agent_trace_generalization.rs apps/bitrouter/tests/workflow_state_replay.rs
git commit -m "refactor: unify predictive route projection"
```

### Task 2: Replace compatibility lookup with exact-to-unknown v1 routing

**Files:**
- Modify: `apps/bitrouter/src/policy_table_router.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Modify: `apps/bitrouter/src/policy_compile.rs`
- Test: `apps/bitrouter/tests/policy_eval_control_plane.rs`
- Test: `apps/bitrouter/tests/trajectory_progress_control.rs`

**Interfaces:**
- Produces: `PolicyTable::tier_for_workflow(primary: &str, unknown_baseline: &str) -> Option<(&str, &str)>`.
- Requires: `validate_predictive_route_keys` accepts only four-segment predictive v1 keys.
- Requires: `validate_predictor_contract` admits only `compiled_predictor_contract()`.
- Removes: observed-route, old-v1, and legacy-fingerprint lookup arguments.

- [ ] **Step 1: Write lookup-order and validation RED tests**

```rust
#[test]
fn task_route_falls_back_only_to_unknown_v1_baseline() {
    let router = router_with_routes([
        ("agent_route/v1|unknown|verify|normal", "balanced"),
        ("agent_trace/v2|tool_followup|normal", "strong"),
    ]);
    let decision = router.decision_for(&review_prompt(), &HeaderMap::new());
    assert_eq!(decision.route_projection, "agent_route/v1|code:review|verify|normal");
    assert_eq!(decision.request_key, "agent_route/v1|unknown|verify|normal");
    assert_eq!(decision.selected_tier.as_deref(), Some("balanced"));
}

#[test]
fn policy_rejects_retired_predictive_route_keys() {
    for key in [
        "agent_route/v1|verify|normal",
        "agent_route/v2|code:review|verify|normal",
    ] {
        assert!(validate_lock_with_route(key).is_err(), "{key}");
    }
}
```

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test -p bitrouter --all-features task_route_falls_back_only_to_unknown_v1_baseline`

Run: `cargo test -p bitrouter --all-features policy_rejects_retired_predictive_route_keys`

Expected: old predictive-v1 or observed route wins, and the current canonical parser admits v2.

- [ ] **Step 3: Simplify lookup and key validation**

Implement exact primary lookup, unknown baseline lookup, then default. Set `route_projection` to the primary key and `request_key` to the matched key. Remove `TaskAwarePredictiveRouteProjection` branches and validate predictive keys directly through the sole `PredictiveRouteProjection` parser.

- [ ] **Step 4: Delete legacy predictor admission**

Remove `PRIOR_V1_PREDICTOR_DIGEST`, the constructed prior descriptor, and the conditional admission. Require `actual == compiled_predictor_contract()` whenever predictive routes exist. Add a test that the exact prior digest is rejected by the current binary.

- [ ] **Step 5: Update certificates and compiler policy-key handling**

Teach baseline certificate lookup to use `PredictiveRouteProjection::unknown_baseline().key()`. Ensure the compiler owns/promotes the primary task-specific key while the certificate records the actual baseline tier.

- [ ] **Step 6: Run policy suites and verify GREEN**

Run: `cargo test -p bitrouter --all-features policy_table_router`

Run: `cargo test -p bitrouter --all-features policy_lock`

Run: `cargo test -p bitrouter --test policy_eval_control_plane --all-features`

Expected: exact, unknown baseline, default, rejection, and current-contract tests pass.

- [ ] **Step 7: Commit**

```bash
git add apps/bitrouter/src/policy_table_router.rs apps/bitrouter/src/policy_lock.rs apps/bitrouter/src/policy_compile.rs apps/bitrouter/tests/policy_eval_control_plane.rs apps/bitrouter/tests/trajectory_progress_control.rs
git commit -m "refactor: remove predictive route compatibility"
```

### Task 3: Remove compatibility evidence from settlement and optimization

**Files:**
- Modify: `apps/bitrouter/src/eval/types.rs`
- Modify: `apps/bitrouter/src/eval/settlement.rs`
- Modify: `apps/bitrouter/src/eval/compiler.rs`
- Modify: `apps/bitrouter/src/eval/store.rs`
- Modify: `apps/bitrouter/src/optimization/runner.rs`
- Modify: `apps/bitrouter/src/optimization/orchestrator.rs`
- Modify: `apps/bitrouter/src/trajectory/evaluation.rs`
- Modify: `apps/bitrouter/src/trajectory/settlement.rs`
- Modify: `apps/bitrouter/src/trajectory/store.rs`
- Modify: `apps/bitrouter/src/output/reports/trajectory.rs`
- Modify: `apps/bitrouter/src/workflow_state/decision.rs`
- Modify: `apps/bitrouter/src/workflow_state/response_observer.rs`
- Modify: `apps/bitrouter/src/workflow_state/reward_feedback.rs`
- Modify: `apps/bitrouter/tests/smithers_reward_loop.rs`
- Modify: `apps/bitrouter/tests/workflow_state_replay.rs`

**Interfaces:**
- Removes: `predictive_v1_fallback_tier` from every runtime and serialized decision contract.
- Retains: primary `route_projection`, matched `request_key`, `selected_tier`, and existing `baseline_tier`.
- Requires: compiler and optimizer group by primary route and derive the governing baseline from `baseline_tier` or the matched route/default.

- [ ] **Step 1: Write evidence and optimizer RED tests**

```rust
#[test]
fn fallback_evidence_uses_general_route_and_baseline_fields() {
    let decision = task_decision_with_unknown_baseline();
    let encoded = serde_json::to_value(&decision).expect("serializes");
    assert_eq!(encoded["route_projection"], "agent_route/v1|code:review|verify|normal");
    assert_eq!(encoded["request_key"], "agent_route/v1|unknown|verify|normal");
    assert_eq!(encoded["baseline_tier"], "balanced");
    assert!(encoded.get("predictive_v1_fallback_tier").is_none());
}
```

Add an optimizer test whose primary task route is absent and whose unknown baseline is strong; the controlled experiment must record strong as its baseline while targeting the primary four-segment v1 key.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test -p bitrouter --all-features fallback_evidence_uses_general_route_and_baseline_fields`

Expected: compatibility field is still serialized and downstream baseline selection depends on it.

- [ ] **Step 3: Remove the compatibility field end-to-end**

Delete the field from `PolicyDecision`, `EvalDecisionRef`, `PendingEvalDecision`, route observations, trajectory records, report records, constructors, validators, semantic digests, and fixtures. In eval baseline selection use the existing route baseline map/default/static tier. In compiler attribution use `decision.baseline_tier` only.

- [ ] **Step 4: Simplify canonical optimization parsing**

Remove `CanonicalPolicyProjection::TaskAwarePredictive`. Parse the sole predictive v1 projection, determine opening semantics from its role, and find the unknown-family baseline before the policy default when constructing an experiment.

- [ ] **Step 5: Run evidence, replay, and optimizer suites and verify GREEN**

Run: `cargo test -p bitrouter --all-features eval::`

Run: `cargo test -p bitrouter --all-features optimization::runner`

Run: `cargo test -p bitrouter --test workflow_state_replay --all-features`

Run: `cargo test -p bitrouter --test smithers_reward_loop --all-features`

Expected: serialization, replay, settlement, compiler, and optimizer tests pass without the removed field.

- [ ] **Step 6: Commit**

```bash
git add apps/bitrouter/src/eval apps/bitrouter/src/optimization apps/bitrouter/src/trajectory apps/bitrouter/src/output/reports/trajectory.rs apps/bitrouter/src/workflow_state/decision.rs apps/bitrouter/src/workflow_state/response_observer.rs apps/bitrouter/src/workflow_state/reward_feedback.rs apps/bitrouter/tests/smithers_reward_loop.rs apps/bitrouter/tests/workflow_state_replay.rs
git commit -m "refactor: simplify routing evidence contract"
```

### Task 4: Publish the unified v1 template and prove the branch

**Files:**
- Modify: `templates/auto-router/policy-lock.yaml`
- Modify: `templates/auto-router/policy-metadata.json`
- Modify: `templates/auto-router/README.md`
- Modify: `skills/bitrouter/references/adaptive-routing.md`
- Modify: `apps/bitrouter/tests/agent_trace_generalization.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`

**Interfaces:**
- Requires: exactly fifteen `agent_route/v1|unknown|<role>|<risk>` baseline entries and the three shipped task overrides under the same v1 namespace.
- Requires: template predictor descriptor equals `compiled_predictor_contract()`.
- Requires: template compiler/certificate digests are regenerated through canonical repository code.

- [ ] **Step 1: Write template RED assertions**

```rust
assert_eq!(predictive_keys.len(), 18);
assert!(predictive_keys.iter().all(|key| {
    key.starts_with("agent_route/v1|") && key.split('|').count() == 4
}));
assert!(predictive_keys.contains("agent_route/v1|unknown|orchestrate|normal"));
assert!(predictive_keys.contains("agent_route/v1|code:review|verify|normal"));
assert!(!template_text.contains("agent_route/v2"));
```

- [ ] **Step 2: Run template tests and verify RED**

Run: `cargo test -p bitrouter --all-features auto_router_template`

Expected: current template still contains fifteen three-segment v1 routes and three v2 routes.

- [ ] **Step 3: Rewrite template and documentation**

Convert all fifteen baselines to `unknown` four-segment v1 keys, convert the three task overrides to v1, and rewrite lookup/migration language. Remove dual-version and compatibility wording. Update the BitRouter skill reference in the same commit.

- [ ] **Step 4: Regenerate canonical digests and certificates**

Use the existing template test/compiler helpers to obtain the current predictor, compiler config, evidence, and certificate digests. Update only values derived from canonical repository code; rerun until the template verifies exactly.

- [ ] **Step 5: Run the real HTTP matrix**

Run: `cargo test -p bitrouter --test agent_trace_generalization --all-features task_aware_cross_harness_routes_equivalent_requests_identically`

Expected: Codex, Claude, Terminus 2, and generic requests produce equivalent four-segment v1 keys and expected tiers.

- [ ] **Step 6: Run full completion gates**

Run: `cargo test --all-features`

Run: `cargo clippy --all-features -- -D warnings`

Run: `cargo fmt -- --check`

Run: `git diff --check`

Run: `cargo run -p bitrouter -- config validate --config templates/auto-router/bitrouter.yaml`

Run: `rg -n 'agent_route/v2|TaskAwarePredictiveRouteProjection|predictive_v1_fallback_tier|predictive_compatibility_routing_key_v1' apps/bitrouter/src templates/auto-router skills/bitrouter`

Expected: all commands pass; the final search returns no matches.

- [ ] **Step 7: Commit**

```bash
git add templates/auto-router skills/bitrouter/references/adaptive-routing.md apps/bitrouter/tests/agent_trace_generalization.rs apps/bitrouter/src/policy_lock.rs
git commit -m "docs: publish unified v1 auto routing"
```

- [ ] **Step 8: Review and update the existing PR**

Generate a diff package from `c8781823` to the final head, run one independent adversarial review, fix any load-bearing findings with RED/GREEN tests, rerun all completion gates, push `codex/next-bitrouter-iteration`, and update draft PR #828 title/body to describe the single breaking v1 contract.
