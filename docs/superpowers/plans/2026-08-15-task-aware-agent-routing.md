# Task-Aware Agent Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add deterministic Code and Agents task-family classification to BitRouter's predictive routing, with sparse v2 policy overrides and complete decision/eval observability.

**Architecture:** The predictor emits a semantic `TaskFamily` alongside the existing next-step role, action, and risk. Confident predictions use `agent_route/v2|<task-family>|<role>|<risk>`; policy lookup falls back to the existing predictive v1 key before observed trace keys. Task-specific cells remain ordinary exact signed routes, so the current compiler and evidence pipeline can optimize them without a Cartesian policy table.

**Tech Stack:** Rust 2024, serde, sha2, BitRouter workflow-state predictor, signed YAML policy locks, Cargo tests/clippy/fmt.

## Global Constraints

- Follow `docs/superpowers/specs/2026-08-15-task-aware-agent-routing-design.md` exactly.
- Never add `#[allow(...)]`.
- Never add public reexports inside public modules.
- Never use `.unwrap()`, `.expect()`, or `panic!()` to make Rust panic.
- Do not add mutable request-time learning, an external classifier, prompt persistence, a fourth model tier, or OpenRouter's five cost bands.
- Task families are semantic; shell execution, file I/O, and tool dispatch remain actions/roles.
- Old `agent_route/v1` locks remain valid and retain their behavior.
- Unknown or low-confidence task classification uses the predictive v1 key.
- Persist only bounded categorical evidence, never raw prompt content.
- Every production behavior change follows RED → GREEN TDD with the focused failing command captured in the task report.
- Before submission run `cargo test --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check`.
- Use Conventional Commit subjects under 60 characters; the PR title follows the same convention.

---

### Task 1: Task-family predictor and canonical v2 projection

**Files:**
- Modify: `apps/bitrouter/src/workflow_state/predictive.rs`
- Modify: `apps/bitrouter/src/workflow_state/online.rs`
- Modify: `apps/bitrouter/src/workflow_state/replay.rs`

**Interfaces:**
- Produces: `TaskFamily`, `TaskAwarePredictiveRouteProjection`, new task-family fields on `PredictiveRouteIR`, and `OnlineWorkflowState::predictive_compatibility_routing_key_v1()`.
- Preserves: `PredictiveRouteProjection` as the exact v1 compatibility type.
- Consumes: existing bounded prompt/history features, `NextStepRole`, and `RouteRisk`.

- [ ] **Step 1: Add failing projection and classification tests**

Add table-driven tests with hand-written expected values. The mutation each test catches is: removing task classification, swapping family precedence, accepting malformed keys, or using harness/private metadata as causal input.

```rust
#[test]
fn task_aware_projection_round_trips_exactly() {
    let projection = TaskAwarePredictiveRouteProjection::new(
        TaskFamily::CodeDebugging,
        NextStepRole::Implement,
        RouteRisk::Guarded,
    );
    assert_eq!(
        projection.key(),
        "agent_route/v2|code:debugging|implement|guarded"
    );
    assert_eq!(
        TaskAwarePredictiveRouteProjection::parse_key(&projection.key()),
        Some(projection)
    );
    assert!(TaskAwarePredictiveRouteProjection::parse_key(
        "agent_route/v2|debugging|implement|guarded"
    )
    .is_none());
}
```

The classifier table must cover the twelve canonical families and `unknown` with literal prompts. Include at least these disambiguation cases:

```text
Fix the panic in the SQL migration runner.       => code:debugging
Review this React pull request for security bugs. => code:review
Plan a multi-step agent handoff.                  => agent:multi_step_planning
Run the shell command and report its output.      => unknown task family; action remains execute/read
```

Also assert that changing `x-bitrouter-harness`, private headers, generated task names, or adapter identity does not change `task_family` or its evidence.

- [ ] **Step 2: Verify the new tests fail for the missing feature**

Run:

```bash
cargo test -p bitrouter --all-features task_family
cargo test -p bitrouter --all-features task_aware_projection
```

Expected: compilation or assertion failure because `TaskFamily`, v2 projection, and task prediction are absent.

- [ ] **Step 3: Implement the minimal deterministic classifier**

Add this public enum with snake-case serde and exact key spelling:

```rust
pub enum TaskFamily {
    CodeGeneration,
    CodeDebugging,
    CodeReview,
    CodeSqlDatabase,
    CodeFrontendUi,
    CodeDevopsConfig,
    CodeRepositoryAnalysis,
    AgentMultiStepPlanning,
    AgentWorkflowExecution,
    AgentWebResearch,
    AgentMemoryOperations,
    AgentGeneral,
    Unknown,
}
```

`TaskFamily::key()` returns the canonical values from the design spec, and
`parse_key()` accepts only those exact values. `Default` is `Unknown`.

Add a task-family scorecard to the same compiled predictor behavior. Use bounded literal term groups, a stable tie order, a minimum score, a minimum evidence count, and a minimum margin. Specific families must outrank `code:generation` and `agent:general`; debugging must outrank SQL/frontend/DevOps when explicit failure/fix evidence is present; review must outrank the subject being reviewed. Do not infer a task family solely from a tool definition or a single action verb.

Extend `PredictiveRouteIR` with backward-compatible defaults:

```rust
#[serde(default)]
pub task_family: TaskFamily,
#[serde(default)]
pub task_family_confidence: f32,
#[serde(default)]
pub task_family_evidence: Vec<PredictiveEvidence>,
```

Set its schema version to `2`. Include task-family terms, weights, thresholds, and tie order in the serialized compiled behavior whose SHA-256 is asserted by `compiled_scorecard_digest()`; update the compiled digest only after the behavior-derived digest test supplies the exact new value.

Add `TaskAwarePredictiveRouteProjection` with strict v2 parsing. Extend `CanonicalPolicyProjection` to parse both predictive v1 and task-aware v2 keys.

- [ ] **Step 4: Make online and replay projections task-aware**

In `OnlineWorkflowState`, retain a stored predictive v1 key. Use v2 as the primary key only when the task family is not `Unknown`; otherwise use v1. Add:

```rust
pub fn predictive_compatibility_routing_key_v1(&self) -> &str;
```

Update replay to build the same primary projection and compare the predicted task family in expected fixtures when supplied. Existing fixtures without task expectations must remain readable.

- [ ] **Step 5: Verify GREEN and commit**

Run:

```bash
cargo test -p bitrouter --all-features workflow_state::predictive::tests
cargo test -p bitrouter --all-features workflow_state::online::tests
cargo test -p bitrouter --all-features workflow_state::replay::tests
cargo fmt -- --check
```

Commit:

```bash
git add apps/bitrouter/src/workflow_state/{predictive.rs,online.rs,replay.rs}
git commit -m "feat: classify agent task families"
```

---

### Task 2: Sparse policy fallback and durable task observability

**Files:**
- Modify: `apps/bitrouter/src/policy_table_router.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Modify: `apps/bitrouter/src/workflow_state/decision.rs`
- Modify: `apps/bitrouter/src/eval/settlement.rs`
- Modify only as required by compile errors: `apps/bitrouter/src/trajectory/evaluation.rs`, `apps/bitrouter/src/trajectory/settlement.rs`, `apps/bitrouter/src/workflow_state/response_observer.rs`, `apps/bitrouter/src/optimization/runner.rs`

**Interfaces:**
- Consumes: Task 1's primary v2 key, predictive v1 compatibility key, `TaskFamily`, confidence, and bounded evidence.
- Produces: exact lookup order `v2 → predictive v1 → observed v2 → observed v1 → default`; decision/eval fields `predicted_task_family` and `task_family_confidence_ppm`; summary map `by_predicted_task_family`.

- [ ] **Step 1: Add failing sparse-fallback tests**

Add real `PolicyTable` decision tests with literal maps. The mutation each catches is: skipping the task override, skipping predictive v1 fallback, or letting observed state preempt the role baseline.

```rust
// Exact v2 route wins.
"agent_route/v2|code:review|verify|normal" => "strong"

// With that v2 entry absent, the same classified request must resolve this
// before either observed trace key.
"agent_route/v1|verify|normal" => "economy"
```

Assert that `route_projection` remains the primary v2 key while `request_key` records the actually matched v1 key. Assert `unknown` never creates an `agent_route/v2` key.

Add policy-lock admission tests proving:

- a v2-only predictive policy without a predictor contract is rejected;
- a malformed task-family key is rejected by canonical projection validation;
- a mixed v1/v2 policy with the compiled contract is accepted.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p bitrouter --all-features task_aware_policy
cargo test -p bitrouter --all-features predictive_v1_fallback
cargo test -p bitrouter --all-features v2_predictor_contract
```

Expected: assertions fail because lookup has no predictive-v1 compatibility candidate and v2 is not recognized as predictive.

- [ ] **Step 3: Implement lookup and contract admission**

Change `PolicyTable::tier_for_workflow` to receive the five candidates in this exact order:

```rust
fn tier_for_workflow<'table, 'key>(
    &'table self,
    predictive_primary: &'key str,
    predictive_v1: &'key str,
    observed_v2: &'key str,
    observed_v1: &'key str,
) -> Option<(&'table str, &'key str)>;
```

Check each exact map entry in the order above, then return the default with the primary key. Update the router call site to pass Task 1's new accessor.

Treat both `agent_route/v1|` and `agent_route/v2|` as predictive namespaces in policy-lock contract validation and predictive single-target marking. Canonical parsing, evidence certificates, and compiler route handling must accept strict v2 keys without accepting arbitrary prefixes.

- [ ] **Step 4: Add and propagate task observability**

Add these serde-defaulted fields wherever the existing predicted role and confidence travel:

```rust
pub predicted_task_family: Option<String>,
pub task_family_confidence_ppm: Option<u32>,
pub task_family_reason_codes: Vec<String>,
```

The router emits the canonical family key, clamps confidence to 0–1,000,000 ppm, and derives reason codes only from the bounded task evidence. `PolicyDecisionRecord` and `PolicyDecisionSummary` expose `by_predicted_task_family`. Eval settlement emits:

```text
predicted_task_family
task_family_confidence_ppm
routing.predicted_task_family
routing.task_family_confidence_ppm
```

Use the existing bounded categorical normalization pattern. Older JSONL and eval records missing these fields must deserialize successfully.

- [ ] **Step 5: Verify GREEN and commit**

Run:

```bash
cargo test -p bitrouter --all-features policy_table_router::tests
cargo test -p bitrouter --all-features policy_lock::tests
cargo test -p bitrouter --all-features workflow_state::decision::tests
cargo test -p bitrouter --all-features eval::settlement::tests
cargo fmt -- --check
```

Commit all Task 2 files with:

```bash
git commit -m "feat: route sparse task overrides"
```

---

### Task 3: Official auto-router template and operator documentation

**Files:**
- Modify: `templates/auto-router/policy-lock.yaml`
- Modify: `templates/auto-router/policy-metadata.json`
- Modify: `templates/auto-router/README.md`
- Modify: `skills/bitrouter/references/adaptive-routing.md`
- Modify: `apps/bitrouter/src/policy_lock.rs` tests
- Modify: `apps/bitrouter/src/policy_compile.rs` tests only when template expectations require it

**Interfaces:**
- Consumes: strict v2 keys and signed predictor contract from Tasks 1–2.
- Produces: a valid official template with a complete v1 baseline and three sparse v2 experimental overrides.

- [ ] **Step 1: Add failing template behavior tests**

Extend the real template-loading tests. Assert these exact sparse routes and tiers:

```text
agent_route/v2|code:review|verify|normal        => strong
agent_route/v2|code:debugging|implement|guarded => strong
agent_route/v2|agent:web_research|mechanical|normal => balanced
```

Also assert there are exactly three v2 routes, every v2 route has one matching certificate, all fifteen v1 baseline routes remain present, and an unlisted v2 cell falls back to its v1 tier at runtime.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p bitrouter --all-features auto_router_template
```

Expected: failure because the template contains no v2 routes and the old predictor digest is bound.

- [ ] **Step 3: Update and validate the signed template**

Add only the three routes above. Keep `economy`, `balanced`, and `strong`; do not add cost bands. Update the predictor digest to Task 1's exact compiled digest. Recompute the compiler config digest, evidence root, and the three new certificate evidence digests using the repository's canonical policy compiler/digest functions; do not handwave or disable validation. Existing certificate semantics remain `owner: compiler`, `source: mixed`, and `verdict: experiment`.

Update metadata and README to explain:

- the twelve task families;
- the v2 key format;
- sparse exact override → v1 role baseline fallback;
- action rows such as shell/file/tool dispatch remain action/role evidence;
- task-specific cells are experimental until promoted by settled eval evidence.

Update the shipped BitRouter skill reference with the same operator-visible key and fallback behavior.

- [ ] **Step 4: Verify GREEN and commit**

Run:

```bash
cargo test -p bitrouter --all-features auto_router_template
cargo test -p bitrouter --all-features policy_compile::tests
cargo run -p bitrouter -- config validate --config templates/auto-router/bitrouter.yaml
cargo fmt -- --check
git diff --check
```

Commit:

```bash
git commit -m "docs: publish task-aware auto template"
```

---

### Task 4: Cross-harness integration and release gates

**Files:**
- Modify: `apps/bitrouter/tests/agent_trace_generalization.rs`
- Modify: `apps/bitrouter/tests/workflow_state_replay.rs` only if a replay fixture is needed
- Modify: production files only for a failing integration behavior, after adding the reproducing test

**Interfaces:**
- Consumes: complete task-aware predictor, sparse fallback, decision records, and template.
- Produces: cross-harness evidence that equivalent Codex, Claude, Terminus, and generic prompts classify and route identically.

- [ ] **Step 1: Add a failing cross-harness integration matrix**

Build equivalent prompt fixtures for Codex, Claude, Terminus, and generic HTTP shapes. Assert literal expected family, role, risk, primary route key, matched route key, and selected tier for at least:

```text
code:debugging / implement / guarded
code:review / verify / normal
agent:web_research / mechanical / normal
unknown task / existing v1 fallback
```

The mutation each catches is harness identity leaking into classification or a v2 route bypassing the established v1 fallback.

- [ ] **Step 2: Verify RED, then make only required integration fixes**

Run:

```bash
cargo test -p bitrouter --all-features task_aware_cross_harness
```

Expected before the test wiring is complete: failure on missing task-aware integration behavior. Implement only the smallest production correction required, then rerun until GREEN.

- [ ] **Step 3: Run focused and repository-wide release gates**

Run in this order:

```bash
cargo test -p bitrouter --all-features task_aware
cargo test -p bitrouter --all-features agent_trace_generalization
cargo test --all-features
cargo clippy --all-features -- -D warnings
cargo fmt -- --check
git diff --check
```

All commands must exit 0 with no new warnings. Record elapsed time and exact test summaries in the task report.

- [ ] **Step 4: Commit**

```bash
git commit -m "test: cover task-aware routing"
```

After this task, generate the whole-branch review package, resolve every Critical/Important finding through the SDD review loop, then use the finishing and GitHub publication skills to push `codex/next-bitrouter-iteration` and open a draft PR.
