# Workflow Role Classifier Corrections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Correct the five adversarially reproduced workflow-role classification failures without changing the unified v1 route interface or adding classifier layers.

**Architecture:** Compute one indexed causal instruction, scope transition evidence to its epoch, and reuse one boundary-aware phrase matcher across deterministic task and role scoring. Preserve the existing scorecard and route lookup architecture while making immediate intent ordering and shell command-head classification explicit.

**Tech Stack:** Rust, BitRouter workflow-state predictor, Cargo unit/integration tests, canonical SHA-256 predictor/template contracts.

## Global Constraints

- Preserve `agent_route/v1|<task-family>|<role>|<risk>` and exact-primary → unknown-family → default policy lookup.
- Add no new classifier, task family, policy tier, route fallback, or dependency.
- Keep changes local to predictor behavior, its tests, and required signed template evidence.
- Do not use `unwrap`, `expect`, `panic!`, or `#[allow(...)]` in production code.
- Follow strict RED → GREEN → REFACTOR cycles for every behavior change.

---

### Task 1: Scope history to the causal instruction epoch

**Files:**
- Modify: `apps/bitrouter/src/workflow_state/predictive.rs`

**Interfaces:**
- Consumes: `Prompt`, `WorkflowStateIR`, existing Terminus normalized action messages.
- Produces: private `CausalInstruction { text: String, message_index: Option<usize> }`; epoch-scoped `HistoryFeatures`.

- [ ] **Step 1: Write failing real-path tests**

Add predictor tests which construct literal message histories and assert:

```rust
assert_eq!(summary_after_old_read.next_step_role, NextStepRole::Finalize);
assert_eq!(implementation_after_old_success.next_step_role, NextStepRole::Implement);
assert_eq!(new_epoch_after_old_failures.route_risk, RouteRisk::Normal);
assert_eq!(normalized_pivot.next_step_role, NextStepRole::Finalize);
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p bitrouter --lib --all-features workflow_state::predictive::tests -- --nocapture
```

Expected: the new tests fail because old read/test/failure evidence still
overrides or guards the new request.

- [ ] **Step 3: Implement the indexed instruction and epoch slice**

Introduce one private causal-instruction value, compute it once in
`predict_next_step`, pass its text into role/task classification, and pass its
message index into `history_features`. Scan structured history from that index.
Use normalized history only if a normalized assistant action occurs after the
selected instruction.

- [ ] **Step 4: Verify GREEN and existing predictor behavior**

Run the focused command from Step 2. Expected: all predictor tests pass.

- [ ] **Step 5: Commit**

```bash
git add apps/bitrouter/src/workflow_state/predictive.rs
git commit -m "fix: scope routing history to current intent"
```

### Task 2: Make explicit role intent boundary-aware and decisive

**Files:**
- Modify: `apps/bitrouter/src/workflow_state/predictive.rs`

**Interfaces:**
- Consumes: scorecard instruction terms and the causal instruction text from Task 1.
- Produces: a shared bounded-term matcher and deterministic mutate/verify ordering.

- [ ] **Step 1: Write failing boundary and explicit-role tests**

Add table-driven assertions with literal expected roles:

```rust
assert_eq!(predict("Implement a new module.").next_step_role, NextStepRole::Implement);
assert_eq!(predict("Verify the result.").next_step_role, NextStepRole::Verify);
assert_eq!(predict("Summarize the result.").next_step_role, NextStepRole::Finalize);
```

For `latest`, `address`, `explanation`, and `await`, assert that the associated
false role reason code is absent. Add `Review the bug fix` → Verify and
`Fix the bug found in code review` → Implement assertions.

- [ ] **Step 2: Verify RED**

Run the focused predictor suite and confirm the new tests fail for the reviewed
weight, substring, and tie behavior.

- [ ] **Step 3: Implement minimal role changes**

Reuse the boundary matcher for instruction terms, remove trailing spaces from
read terms, set ordinary mutation/verification/finalize weights to five, and
when mutate and verify both match retain only the one whose first bounded
occurrence comes first.

- [ ] **Step 4: Verify GREEN**

Run the focused predictor suite. Expected: all tests pass with no changed route
schema.

- [ ] **Step 5: Commit**

```bash
git add apps/bitrouter/src/workflow_state/predictive.rs
git commit -m "fix: resolve explicit workflow role intent"
```

### Task 3: Order review/debugging intent and shell command heads

**Files:**
- Modify: `apps/bitrouter/src/workflow_state/predictive.rs`

**Interfaces:**
- Consumes: shared bounded-term position lookup from Task 2.
- Produces: first-intent task-family precedence and bounded read/test command-head classification.

- [ ] **Step 1: Write failing task-family and action tests**

Add literal behavior assertions:

```rust
assert_eq!(family("Review the bug fix."), TaskFamily::CodeReview);
assert_eq!(family("Fix the bug found in code review."), TaskFamily::CodeDebugging);
assert_eq!(action("rg 'cargo test' README.md"), ObservedAction::Read);
assert_eq!(action("cargo test -p bitrouter"), ObservedAction::Test);
```

- [ ] **Step 2: Verify RED**

Run the focused predictor suite. Expected: fix-first review wording and the read
command containing test text fail under existing precedence.

- [ ] **Step 3: Implement first-intent and command-head ordering**

Apply review precedence only when its first bounded intent precedes the first
debugging intent. Match trimmed command prefixes with an end-or-whitespace
boundary and check read commands before tests.

- [ ] **Step 4: Verify GREEN and cross-harness behavior**

```bash
cargo test -p bitrouter --lib --all-features workflow_state::predictive::tests
cargo test -p bitrouter --test agent_trace_generalization --all-features
```

- [ ] **Step 5: Commit**

```bash
git add apps/bitrouter/src/workflow_state/predictive.rs
git commit -m "fix: order routing intent and command heads"
```

### Task 4: Refresh the signed predictor and template evidence

**Files:**
- Modify: `apps/bitrouter/src/workflow_state/predictive.rs`
- Modify: `templates/auto-router/policy-lock.yaml`
- Modify: `templates/auto-router/policy-metadata.json`

**Interfaces:**
- Consumes: deterministic compiled behavior and canonical template digest tests.
- Produces: exact current predictor descriptor and matching compiler/evidence hashes.

- [ ] **Step 1: Write or extend digest mutation assertions**

Assert that changing each new algorithm-version component changes the compiled
digest, using the existing cloned-behavior digest test pattern.

- [ ] **Step 2: Verify RED**

Run predictor and auto-router template tests. Expected: the algorithm-version
assertions or stale template descriptor/hashes fail with the new behavior.

- [ ] **Step 3: Refresh only canonical evidence values**

Bump the affected algorithm versions. Use the failing canonical digest output
from the repository tests to update the predictor descriptor, compiler digest,
per-route evidence digests, and evidence root without changing routes or tiers.

- [ ] **Step 4: Verify GREEN**

```bash
cargo test -p bitrouter --lib --all-features workflow_state::predictive::tests
cargo test -p bitrouter --lib --all-features policy_lock::tests::auto_router_template
cargo run -p bitrouter -- config validate --config templates/auto-router/bitrouter.yaml
```

- [ ] **Step 5: Commit**

```bash
git add apps/bitrouter/src/workflow_state/predictive.rs templates/auto-router/policy-lock.yaml templates/auto-router/policy-metadata.json
git commit -m "chore: refresh workflow predictor evidence"
```

### Task 5: Review, verify, and publish

**Files:**
- Review: all files changed since the plan base SHA.

**Interfaces:**
- Consumes: Tasks 1–4 commits.
- Produces: reviewed, fully verified, pushed branch.

- [ ] **Step 1: Run an independent read-only code review**

Request review against this plan and fix every Critical or Important finding
with a new RED → GREEN cycle.

- [ ] **Step 2: Run repository-required verification**

```bash
cargo test --all-features
cargo clippy --all-features -- -D warnings
cargo fmt -- --check
git diff --check
```

- [ ] **Step 3: Audit every requirement**

Confirm all five adversarial cases have direct regression tests, route keys and
fallback order are unchanged, template contracts equal the compiled contract,
and the worktree contains only intentional committed changes.

- [ ] **Step 4: Push the named branch**

```bash
git push origin codex/next-bitrouter-iteration
```

Do not force-push. Report the pushed commit and verification evidence.
