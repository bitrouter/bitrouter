# Routing Evolution Evidence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce conservative Terminal-Bench route evidence through the external Eval Exchange skill, harden generic inconclusive aggregation, and make balanced the ordinary progress-guard tier without publishing an active policy.

**Architecture:** Keep Rust benchmark-neutral and add one defensive `EvalVerdict::Inconclusive` aggregation gate. Put Harbor parsing, exception taxonomy, unique task attribution, packet generation, evidence matrices, and recommendation gates in a deterministic standard-library Python adapter under `skills/evaluating-bitrouter-routes`; change the starter lock only through existing progress-guard configuration and refresh its canonical provenance through repository tests.

**Tech Stack:** Rust, Tokio/SQLite unit tests, Python 3 standard library, `unittest`, JSON/JSONL/CSV, YAML template validation, Git.

## Global Constraints

- Do not add Terminal-Bench, Harbor, provider, network, auth, rate-limit, or transport exception concepts to Rust or SDK schemas.
- Keep the main program's evaluator semantics limited to generic pass, fail, inconclusive, quality, cost, latency, and hard-violation credit.
- An inconclusive result never increments eligible episodes, independent tasks, pass/fail weight, total quality weight, or quality-scoped critical violations; explicit cost and latency credit may aggregate.
- Never duplicate a task- or episode-level reward across requests; positive quality credit requires one fixed, auditable route cell and one deterministic representative decision.
- Direct economy adoption requires actual economy use, at least five independent conclusive tasks, pass rate at least 80%, zero critical violations, unique attribution, no later strong recovery dependency, and zero excluded non-task-error contamination.
- Keep non-causal observational screening separate from Eval Exchange quality: it may name controlled-validation candidates, but `quality_credit_eligible` remains false and it cannot drive publication or automatic template edits.
- Balanced low-risk cells may be economy experiment candidates but remain balanced; undersampled and ambiguous cells retain their current route.
- Change the template progress guard to `protected_tiers: [strong, balanced]` while retaining `escalation_tier: strong`.
- Do not publish an active policy or run/resume a paid benchmark.
- Do not guess active economy routes when the external analysis artifact is absent.
- Refresh derived template digests and certificates only through the repository's canonical method.
- Do not use `unwrap`, `expect`, `panic!`, or `#[allow(...)]` in production Rust.
- Preserve existing history, do not force-push, and push only `codex/next-bitrouter-iteration` after all verification passes.

---

### Task 1: Make inconclusive quality credit inert

**Files:**
- Modify: `apps/bitrouter/src/eval/compiler.rs`

**Interfaces:**
- Consumes: `EvaluationResult { verdict, metrics, hard_violations, decision_credit }`.
- Produces: `EvalEvidenceSnapshot::route_evidence()` that aggregates independent cost/latency for inconclusive results while withholding every quality-derived aggregate.

- [ ] **Step 1: Write the failing compiler test**

Add `inconclusive_credit_cannot_create_quality_evidence` using one real stored
subject and an inconclusive result with:

```rust
result.metrics.insert(
    "cost.usd_micros".into(),
    MetricValue::new(420, MetricUnit::MicroUsd),
);
result.metrics.insert(
    "latency.ms".into(),
    MetricValue::new(315, MetricUnit::Milliseconds),
);
result.hard_violations.push("quality.critical".into());
result.decision_credit.insert(
    "decision-a".into(),
    DecisionCredit {
        weight_ppm: 1_000_000,
        metric_ids: BTreeSet::from([
            "quality.pass".into(),
            "quality.critical".into(),
            "cost.usd_micros".into(),
            "latency.ms".into(),
        ]),
    },
);
```

Assert literal zero values for `eligible_episodes`, `independent_tasks`,
`total_weight_ppm`, pass/fail weights, and critical violations, plus means
`Some(420)` and `Some(315)`.

- [ ] **Step 2: Verify RED**

```bash
cargo test -p bitrouter --lib --all-features eval::compiler::tests::inconclusive_credit_cannot_create_quality_evidence -- --exact --nocapture
```

Expected: FAIL because current code counts the episode/task/total quality weight
and the credited quality violation despite the inconclusive verdict.

- [ ] **Step 3: Implement the generic verdict gate**

Make quality and hard-violation aggregation conditional on a conclusive verdict:

```rust
let conclusive = record.result.verdict != EvalVerdict::Inconclusive;
let quality_credited = conclusive && credit.includes("quality.pass");
let credited_violations = if conclusive {
    record
        .result
        .hard_violations
        .iter()
        .filter(|violation| credit.includes(violation))
        .count()
} else {
    0
};
```

Do not gate `cost.usd_micros` or `latency.ms` on verdict.

- [ ] **Step 4: Verify GREEN and regression coverage**

```bash
cargo test -p bitrouter --lib --all-features eval::compiler::tests -- --nocapture
```

- [ ] **Step 5: Commit**

```bash
git add apps/bitrouter/src/eval/compiler.rs
git commit -m "fix: withhold inconclusive quality evidence"
```

### Task 2: Parse Harbor outcomes and build generic packets

**Files:**
- Create: `skills/evaluating-bitrouter-routes/scripts/terminal_bench_route_evidence.py`
- Create: `skills/evaluating-bitrouter-routes/scripts/tests/test_terminal_bench_route_evidence.py`
- Create: `skills/evaluating-bitrouter-routes/scripts/tests/fixtures/run/jobs/job/pass/result.json`
- Create: `skills/evaluating-bitrouter-routes/scripts/tests/fixtures/run/jobs/job/provider/result.json`
- Create: `skills/evaluating-bitrouter-routes/scripts/tests/fixtures/run/decisions.jsonl`

**Interfaces:**
- Consumes: `--run-dir PATH`, `--decisions PATH`, `--output-dir PATH`, with Harbor `result.json` files and router decision JSONL carrying exact full-prefix/task-description and ingress digest joins.
- Produces: `packets.jsonl`, `task-evidence.jsonl`, and deterministic evidence digests compatible with Eval Exchange schema version 1.

- [ ] **Step 1: Write failing classification and packet tests**

Use standard-library `unittest` and import the script module. Assert these
literal behaviors:

```python
self.assertEqual(classify_outcome(pass_result).verdict, "pass")
self.assertEqual(classify_outcome(provider_result).verdict, "inconclusive")
self.assertEqual(classify_outcome(provider_result).excluded_error_kind, "provider")
self.assertEqual(packet["result"]["evaluator"]["kind"], "task_native")
self.assertEqual(packet["subject"]["scope"], "task")
```

Add one literal fixture for each non-task class: provider, network, auth,
rate-limit, and transport. Add an overlapping-marker fixture and assert the
documented ordered rule id wins, so broad provider text cannot mask a more
specific auth, rate-limit, network, or transport classification.

The pass fixture contains two decisions with one route projection and one
selected tier; assert `decision_credit` contains exactly the earliest decision
and weight `1000000`. The provider fixture has no verifier reward and assert its
quality credit map is empty.

- [ ] **Step 2: Verify RED**

```bash
python3 -m unittest discover -s skills/evaluating-bitrouter-routes/scripts/tests -v
```

Expected: FAIL because the adapter module and output types do not exist.

- [ ] **Step 3: Implement deterministic parsing and classification**

Implement typed dataclasses and functions with these stable signatures:

```python
def load_trials(run_dir: Path) -> list[Trial]: ...
def load_decisions(path: Path) -> list[Decision]: ...
def classify_outcome(raw: dict[str, object]) -> Outcome: ...
def join_decisions(trial: Trial, decisions: list[Decision]) -> list[Decision]: ...
def build_packet(trial: Trial, decisions: list[Decision], outcome: Outcome) -> dict[str, object]: ...
```

Use an ordered `ERROR_RULES` tuple with versioned rule ids. Match only in this
external module, test every class and precedence, and keep classification
details out of Rust. A valid binary verifier reward wins over a recoverable
request failure; otherwise non-task exceptions are inconclusive and counted
separately.

Join only by exact content identities and the exact ingress digest. Never use
capture/agent-execution time proximity. Rows without a unique exact mapping are
preserved as unassigned observations and cannot receive quality credit.

Derive one positive quality mapping only when all joined decisions have ids,
one policy, one route projection, and one selected tier. Choose the earliest
`(captured_at, decision_id)` as representative. Ambiguous tasks retain copied
decisions but use verdict `inconclusive` and empty credit.

Create redacted evidence entries with SHA-256 digests of the input result and
classification record. Compute `evidence_digest` as SHA-256 of compact UTF-8
JSON over evidence sorted by `evidence_id`, matching `serde_json::to_vec` field
order. Derive stable evaluator/config/idempotency values from versioned public
inputs only.

- [ ] **Step 4: Verify GREEN and deterministic CLI output**

```bash
python3 -m unittest discover -s skills/evaluating-bitrouter-routes/scripts/tests -v
tmpdir=$(mktemp -d)
python3 skills/evaluating-bitrouter-routes/scripts/terminal_bench_route_evidence.py \
  --run-dir skills/evaluating-bitrouter-routes/scripts/tests/fixtures/run \
  --decisions skills/evaluating-bitrouter-routes/scripts/tests/fixtures/run/decisions.jsonl \
  --output-dir "$tmpdir"
diff -u skills/evaluating-bitrouter-routes/scripts/tests/fixtures/expected/packets.jsonl "$tmpdir/packets.jsonl"
```

- [ ] **Step 5: Commit**

```bash
git add skills/evaluating-bitrouter-routes/scripts
git commit -m "feat: adapt terminal benchmark evidence"
```

### Task 3: Generate the auditable experience matrix and gates

**Files:**
- Modify: `skills/evaluating-bitrouter-routes/scripts/terminal_bench_route_evidence.py`
- Modify: `skills/evaluating-bitrouter-routes/scripts/tests/test_terminal_bench_route_evidence.py`
- Create: `skills/evaluating-bitrouter-routes/scripts/tests/fixtures/expected/matrix.json`
- Create: `skills/evaluating-bitrouter-routes/scripts/tests/fixtures/expected/matrix.csv`
- Create: `skills/evaluating-bitrouter-routes/scripts/tests/fixtures/expected/packets.jsonl`

**Interfaces:**
- Consumes: normalized task evidence from Task 2.
- Produces: `matrix.json` and `matrix.csv` sorted by policy/route with strict recommendation and isolated observational-screening fields.

- [ ] **Step 1: Write failing literal matrix tests**

Build fixture rows for five unique economy passes, one economy failure excluded
as provider error, five normal-risk balanced tasks, one mixed-tier recovery, and
one multi-cell ambiguous task. Assert:

```python
self.assertEqual(economy["independent_tasks"], 5)
self.assertEqual(economy["pass_rate_ppm"], 1_000_000)
self.assertEqual(economy["excluded_non_task_errors"], 1)
self.assertEqual(economy["active_recommendation"], "economy")
self.assertFalse(economy["economy_experiment_candidate"])
self.assertEqual(balanced["active_recommendation"], "retain")
self.assertTrue(balanced["economy_experiment_candidate"])
self.assertTrue(balanced["controlled_validation_candidate"])
self.assertFalse(balanced["quality_credit_eligible"])
self.assertEqual(balanced["screening_reason"], "balanced_normal_observational")
self.assertGreater(ambiguous["attribution_ambiguities"], 0)
```

Assert every required CSV column and the exact deterministic JSON fixture. Add
a task whose strict whole-task attribution is ambiguous but whose associated
balanced cell passes the observational screen; assert it remains inconclusive,
receives no packet quality credit, and appears only as a controlled-validation
candidate.

- [ ] **Step 2: Verify RED**

Run the Task 2 unittest command. Expected: FAIL because matrix aggregation and
recommendation gates are absent.

- [ ] **Step 3: Implement matrix aggregation and conservative gates**

Parse canonical route keys into task family, role, and risk. Aggregate unique
task/episode ids, pass/fail/inconclusive, actual/static tier counters,
cost/latency sums and sample counts, guard promotions, excluded error rule ids,
critical violations, and ambiguity/recovery flags.

Implement exact predicates:

```python
adoptable = (
    actual_tiers == {"economy"}
    and conclusive_tasks >= 5
    and pass_rate_ppm >= 800_000
    and critical_violations == 0
    and recovery_dependencies == 0
    and attribution_ambiguities == 0
    and excluded_non_task_errors == 0
)
experiment = (
    quality_credit_eligible
    and actual_tiers == {"balanced"}
    and risk == "normal"
    and conclusive_tasks >= 5
    and pass_rate_ppm >= 800_000
    and critical_violations == 0
    and recovery_dependencies == 0
    and attribution_ambiguities == 0
    and excluded_non_task_errors == 0
)
```

Map grades to `adoptable`, `experiment`, `insufficient`, or `inconclusive`.
Never edit a policy file from this script.

Separately compute `controlled_validation_candidate` from exact route/task
associations. Require actual economy exposure or a normal-risk balanced cell,
at least five distinct passing-task associations, zero route-level non-task
errors, zero recovery dependency, and zero critical violations. Set
`quality_credit_eligible: false` for an observational-only row and emit
`screening_reason` as `economy_exposure_observational` or
`balanced_normal_observational`. Never copy associated task rewards into packet
credit or use screening to set `active_recommendation`.

- [ ] **Step 4: Verify GREEN and stable artifacts**

Run the unittests and the deterministic CLI/diff commands from Task 2, then
diff both matrix outputs against their expected fixtures.

- [ ] **Step 5: Commit**

```bash
git add skills/evaluating-bitrouter-routes/scripts
git commit -m "feat: grade route evolution evidence"
```

### Task 4: Publish the external evaluator workflow in the skill

**Files:**
- Modify: `skills/evaluating-bitrouter-routes/SKILL.md`
- Modify: `skills/evaluating-bitrouter-routes/agents/openai.yaml`
- Modify: `skills/evaluating-bitrouter-routes/references/eval-exchange.md`
- Create: `skills/evaluating-bitrouter-routes/references/terminal-bench-harbor.md`

**Interfaces:**
- Consumes: the adapter CLI and fixed contracts from Tasks 2–3.
- Produces: discoverable instructions for evaluating Harbor/Terminal-Bench runs without leaking benchmark semantics into BitRouter.

- [ ] **Step 1: Record the existing skill gap as an executable fixture**

Run the fixture workflow and validate these consumer-visible outputs rather
than grepping prose: provider failures are inconclusive, multi-cell reward is
withheld, one representative receives quality credit, and both JSON/CSV
matrices include every required column. Preserve this executable baseline; the
human-facing prose is reviewed directly rather than with a source-text test.

- [ ] **Step 2: Write the concise skill and detailed reference**

Keep `SKILL.md` as the entry workflow. Link directly to
`references/terminal-bench-harbor.md` when the input is a Harbor run. Document:

```bash
python3 scripts/terminal_bench_route_evidence.py \
  --run-dir /path/to/run \
  --decisions /path/to/policy-decisions.jsonl \
  --output-dir /path/to/evolution-analysis
```

The reference defines exact non-temporal joins, input fields, taxonomy rule ids,
verifier precedence, unique attribution, strict matrix gates, the isolated
non-causal controlled-validation screen, packet submission handoff, and the
prohibition on active publication.

Update the Eval Exchange reference so inconclusive results always withhold
quality credit even if an old packet supplies a positive mapping, while cost
and latency remain independent. Regenerate `agents/openai.yaml` with the
skill-creator canonical generator and an explicit `$evaluating-bitrouter-routes`
default prompt.

- [ ] **Step 3: Validate the skill and execute its fixtures**

```bash
python3 /Users/archer/.codex/skills/.system/skill-creator/scripts/quick_validate.py skills/evaluating-bitrouter-routes
python3 -m unittest discover -s skills/evaluating-bitrouter-routes/scripts/tests -v
```

- [ ] **Step 4: Commit**

```bash
git add skills/evaluating-bitrouter-routes
git commit -m "docs: teach conservative route evaluation"
```

### Task 5: Protect balanced progress and refresh canonical provenance

**Files:**
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Modify: `templates/auto-router/policy-lock.yaml`
- Modify: `templates/auto-router/README.md`

**Interfaces:**
- Consumes: existing generic `ProgressGuardPolicy` configuration.
- Produces: starter template with protected tiers `strong` and `balanced`, escalation tier `strong`, and canonical compiler/evidence provenance.

- [ ] **Step 1: Change the template behavior test first**

Update the existing template assertion to require:

```rust
assert_eq!(
    guard.protected_tiers,
    BTreeSet::from(["balanced".into(), "strong".into()]),
);
assert_eq!(guard.escalation_tier, "strong");
```

- [ ] **Step 2: Verify RED**

```bash
cargo test -p bitrouter --lib --all-features policy_lock::tests::auto_router_template_lock_is_bound_and_canonical -- --exact --nocapture
```

Expected: FAIL because the current template protects only strong.

- [ ] **Step 3: Change only the configuration and prose**

Add `balanced` to `protected_tiers`. Explain in the README that ordinary
balanced progress remains unpromoted while structural escalation still selects
strong. Do not change program protocol, defaults, route table, or active policy.

- [ ] **Step 4: Refresh digests using the canonical repository test**

Run:

```bash
cargo test -p bitrouter --lib --all-features policy_lock::tests::auto_router_template_routes_have_deterministic_compiler_certificates -- --exact --nocapture
```

Use a repository-local canonical refresh helper if present. If the test reports
the derived expected compiler digest, update only that reported value and rerun.
Do not calculate or type a digest from an ad-hoc algorithm. Per-route evidence
digests and evidence root remain unchanged unless the canonical test reports a
mismatch.

- [ ] **Step 5: Verify template and config behavior**

```bash
cargo test -p bitrouter --lib --all-features policy_lock::tests::auto_router_template -- --nocapture
cargo test -p bitrouter --test agent_trace_generalization --all-features auto_template -- --nocapture
cargo run -p bitrouter -- config validate --config templates/auto-router/bitrouter.yaml
```

- [ ] **Step 6: Commit**

```bash
git add apps/bitrouter/src/policy_lock.rs templates/auto-router/policy-lock.yaml templates/auto-router/README.md
git commit -m "feat: make balanced the progress tier"
```

### Task 6: Integrate available analysis without guessing policy

**Files:**
- Inspect: `/Users/archer/.codex/worktrees/9907/bitrouter-ws/artifacts/tb21-v1-full1-20260816T231420Z/evolution-analysis-v1/`
- Modify only if justified: `templates/auto-router/policy-lock.yaml`
- Modify only if justified: `templates/auto-router/README.md`
- Write: `.superpowers/sdd/2026-08-17-routing-evolution-evidence/analysis-integration.md`

**Interfaces:**
- Consumes: external analysis agent matrix/recommendations and the approved direct-adoption gate.
- Produces: an audit ruling for each recommended cell; active route edits only for cells that independently satisfy every gate.

- [ ] **Step 1: Inspect the analysis directory at the integration boundary**

If absent, record `analysis artifact absent; no active economy route changes` in
the SDD report and continue. If present, record source file digests and compare
every proposed active/economy-experiment cell to the adapter matrix.

- [ ] **Step 2: Apply only proven active changes**

For each proposed active economy cell, require literal evidence for actual
economy use, five independent conclusive tasks, pass rate >= 800,000 ppm, zero
critical violations, zero recovery dependencies, and zero attribution
ambiguities, plus zero excluded non-task errors. Reject any incomplete cell and
retain its existing route.

Experiment and controlled-validation candidates are reported only; do not
change their route. Confirm observational candidates have
`quality_credit_eligible: false` and never treat their task associations as Eval
quality. If no strict cell passes, make no route edit and do not refresh
per-route certificates.

- [ ] **Step 3: Validate any justified route edit canonically**

If and only if a route changes, update its compiler-owned certificate through
the canonical template method, run all Task 5 template checks, and commit with:

```bash
git commit -m "feat: adopt proven economy route evidence"
```

### Task 7: Self-review, full verification, and branch completion

**Files:**
- Review: every file changed since `88328d0276877804b37f82c61923b31ffbfde143`.
- Write: `.superpowers/sdd/2026-08-17-routing-evolution-evidence/final-review.md`

**Interfaces:**
- Consumes: Tasks 1–6 commits and verification output.
- Produces: complete SDD report, clean pushed branch, and concise status handoff.

- [ ] **Step 1: Run a requirements and code self-review**

Inspect the complete diff for benchmark leakage into Rust, quality duplication,
ambiguous credit, incorrect direct-adoption gates, hand-authored hashes,
unrelated changes, secrets, and ignored expected files. Record findings and
their resolutions in `final-review.md`.

- [ ] **Step 2: Run focused skill/schema/template verification**

```bash
python3 /Users/archer/.codex/skills/.system/skill-creator/scripts/quick_validate.py skills/evaluating-bitrouter-routes
python3 -m unittest discover -s skills/evaluating-bitrouter-routes/scripts/tests -v
cargo test -p bitrouter --lib --all-features eval::compiler::tests -- --nocapture
cargo test -p bitrouter --lib --all-features policy_lock::tests::auto_router_template -- --nocapture
cargo test -p bitrouter --test agent_trace_generalization --all-features auto_template -- --nocapture
cargo run -p bitrouter -- config validate --config templates/auto-router/bitrouter.yaml
```

- [ ] **Step 3: Run repository-required full verification**

```bash
cargo test --all-features
cargo clippy --all-features -- -D warnings
cargo fmt -- --check
git diff --check
```

- [ ] **Step 4: Verify committed scope and clean state**

```bash
git diff --stat 88328d0276877804b37f82c61923b31ffbfde143..HEAD
git status --short --branch
git log --oneline 88328d0276877804b37f82c61923b31ffbfde143..HEAD
```

Commit any expected final report-independent repository changes with a
conventional message under 60 characters. The ignored SDD report remains the
full local audit record.

- [ ] **Step 5: Push without rewriting history**

```bash
git push origin codex/next-bitrouter-iteration
```

Verify local HEAD equals `origin/codex/next-bitrouter-iteration` and the
worktree is clean. Return only status, commits, key tests, and one risk/concern
line; refer to the SDD report for full details.

---

## Fix round 1 — rejected review findings

The five findings below are one atomic remediation wave. Do not push a subset.

### Fix 1: Remove benchmark-specific serving code

**Files:** `apps/bitrouter/src/main.rs`,
`apps/bitrouter/src/workflow_state/reward.rs`,
`apps/bitrouter/tests/workflow_state_replay.rs`, `docs/CLI.md`, and generic
production comments in `apps/` / `crates/`.

1. RED: execute a boundary scan that finds the Harbor CLI/parser symbols in
   production Rust and the documented command.
2. Delete `WorkflowStateAction::HarborOutcomes`, its handler, recursive result
   parser/helpers, and parser-specific Rust tests.
3. Preserve generic `BenchmarkOutcomeRecord` JSONL and strict request-id joins.
4. Rewrite benchmark-derived production comments generically; retain the
   Terminus 2 extractor.
5. GREEN: boundary scan has no prohibited production/documentation symbols and
   focused workflow-state/main tests pass.

### Fix 2: Make request coverage fail closed

**Files:** external adapter and its tests/fixtures.

1. RED: production `run()` fixture with two same-task traces where only one
   joins currently emits positive quality.
2. Introduce a coverage ledger for trace→task, trace→decision, and
   trace→request-outcome joins plus unconsumed inputs.
3. Propagate task-local reasons to the task; propagate unattributable defects to
   a batch quality block. Packets and all recommendations remain inconclusive
   under either block.
4. GREEN: partial, ambiguous, duplicate, missing-identity, and unconsumed cases
   retain audit rows but have zero quality/recommendation.

### Fix 3: Join authoritative request outcomes

1. RED: an end-to-end fixture using the real request-outcome schema currently
   misses its exclusions or succeeds without complete coverage.
2. Require `--request-outcomes`; stream rows keyed uniquely by physical
   `request_id`, require an explicit nullable `error`, and attach through the
   trace/ingress chain.
3. Aggregate request cost/latency when present and classify all request errors
   only in the external taxonomy.
4. GREEN: redacted fixture counts all exclusions; missing files/rows block
   quality. The final 1,306-row artifact replay reports 303 exclusions.

### Fix 4: Propagate task recovery and trusted critical state

1. RED: a multi-cell task whose later cell selects strong currently leaves an
   earlier cell recovery-clean; a result without critical evidence currently
   defaults to zero and can screen.
2. Compute recovery once across the task and propagate it to every associated
   cell.
3. Parse non-negative critical-violation evidence only from an explicit trusted
   result/eval field. Record known/unknown; unknown blocks strict and
   observational recommendations.
4. GREEN: production run→matrix fixtures cover cross-cell recovery and
   known-zero/unknown/nonzero critical gates.

### Fix 5: Strengthen identity and real validation

1. RED: cover duplicate decision/ingress/result/input rows, missing
   `exact_task_id`, repeated attempt identity, non-finite reward, and different
   attempt collision.
2. Derive source, attribution, eval, subject, and idempotency identities from
   versioned content plus canonical task and trial/attempt identity. Keep matrix
   task counts canonical-task distinct.
3. Automatically pass every fixture packet through the existing real BitRouter
   `eval subject seal`, `eval subject put`, and `eval result submit` commands;
   do not add benchmark logic or a new schema to Rust.
4. Add/retain explicit Rust coverage for duplicate Eval decision rejection.
5. Bound input bytes/lines/rows and escape spreadsheet formula-leading CSV text.

### Fix-round verification and completion

1. Run the full adapter tests and real packet validator loop.
2. Replay the final artifact with run directory, traces, decisions, and request
   outcomes; require exclusions=303, active=0, controlled candidate=0.
3. Run focused Eval, template, workflow-state/replay, and CLI tests.
4. Run `cargo test --all-features`, workspace/all-targets clippy with warnings
   denied, fmt, committed diff check, config validation, skill validation, and
   `dist-helper check`.
5. Update the SDD ledger/review, commit all expected changes, fetch, and
   ordinary-push the same branch only when every gate is green.

---

## Fix round 2 — unknown errors and canonical subjects

This is one narrow atomic remediation wave. It modifies the external adapter
and regression tests only; production Rust remains benchmark-neutral.

### Fix 2.1: Fail closed on explicit unclassified errors

**Files:** external adapter, matrix contract, skill references, Python tests,
and one generic compiler integration regression.

1. RED: generate five exact, passing, economy tasks whose authoritative
   request outcomes contain `mystery_upstream_fault`. Run the production
   adapter and then the real `eval subject seal`, `subject put`, `result
   submit`, snapshot, and compiler path. Record that the old code produces five
   quality tasks and an economy recommendation.
2. Preserve every explicit non-null authoritative error as a `RequestError`.
   Known taxonomy categories keep the existing exclusion accounting; unknowns
   remain category-less with rule `unclassified.v1` and receive separate
   counts/reasons.
3. Make unknown-error contamination block packet quality, strict route
   recommendations, and controlled-validation screening while retaining the
   terminal verifier outcome at zero quality weight.
4. GREEN: the five-task production pipeline reports quality 0, active 0, and
   controlled 0; no unknown is counted as a known provider category.

### Fix 2.2: Deduplicate attempts by canonical task subject

**Files:** external adapter, Python identity tests, and the same generic
compiler integration regression.

1. RED: run two exact passing attempts for one canonical task. Confirm the old
   packets have distinct `subject_id` values and the generic compiler reports
   two independent tasks while the adapter matrix reports one.
2. Keep eval/result/evidence/idempotency identity attempt-specific. Derive
   `subject_id` separately from canonical task plus run, full source, and policy
   namespace, excluding trial/result attempt identity.
3. GREEN: the attempts retain distinct eval ids and results, share one subject
   id, and both adapter matrix and compiler report one independent task. No
   recommendation reaches the five-task gate.

### Fix-round-2 verification and completion

1. Run focused Python RED/GREEN tests and the real generic ingestion/compiler
   integration test.
2. Replay the final real artifact; require exact joins=1,306, known
   exclusions=303, unknown exclusions=0, active=0, and controlled=0.
3. Run the complete adapter suite, skill/schema/config/template checks,
   focused Eval/workflow tests, `cargo test --all-features`, workspace
   all-target Clippy with warnings denied, format, and diff checks.
4. Append RED/GREEN and verification evidence to the ignored SDD ledger and
   final review. Commit the complete wave separately, fetch, and ordinary-push
   `codex/next-bitrouter-iteration` only after every gate is green.
