# Strong-Tier Cost-Budget Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce reproducible external cost-budget evidence and promote the generic guarded-mechanical route to strong while preserving a 40%–50% estimated savings target.

**Architecture:** A skill-owned Terminal-Bench adapter performs strict request/decision joining and token-preserving repricing outside the product. The BitRouter product change is limited to one generic auto-router template cell plus canonically regenerated signed evidence and explanatory prose.

**Tech Stack:** Python 3 standard library, Rust/Cargo, YAML policy locks, BitRouter config validation, SHA-256 manifests.

**Spec:** `docs/superpowers/specs/2026-08-19-strong-tier-cost-budget-design.md`

## Global Constraints

- Use only the 22-task fully clean cohort: numeric reward, no Harbor exception, and no physical request error.
- Provider/network/auth/rate-limit/transport failures receive zero optimization credit.
- Keep all Terminal-Bench parsing and task knowledge outside BitRouter runtime code.
- Keep progress guard `protected_tiers: [strong, balanced]` unchanged.
- Preserve each matched control attempt separately; never average controls.
- The counterfactual fixes observed token counts and must be labeled non-causal.
- Follow RED → GREEN for every executable behavior.
- Commit intentionally and push only after fresh full verification.

---

### Task 1: External strong-tier budget planner

**Files:**
- Create: `skills/evaluating-bitrouter-routes/scripts/terminal_bench_strong_tier_plan.py`
- Create: `skills/evaluating-bitrouter-routes/scripts/tests/test_terminal_bench_strong_tier_plan.py`
- Modify: `skills/evaluating-bitrouter-routes/SKILL.md`
- Modify: `skills/evaluating-bitrouter-routes/references/terminal-bench-harbor.md`

**Interfaces:**
- Consumes: `validity-audit.json`, `current-request-model-join.jsonl`, a BitRouter daemon log, repeated `--control-attempt-cost`, a target policy key, and four strong-tier rates.
- Produces: `summary.json`, `route-cells.csv`, `report.md`, and `sha256-manifest.json` in an explicitly supplied output directory.

- [ ] **Step 1: Record the skill baseline failure**

Run an application scenario without the new planner instructions: ask for a
40%–50% strong-promotion plan from the frozen artifacts while forbidding
averaged controls and non-task-error evidence. Record any missing exact-join,
repricing, or fail-closed behavior in the SDD ledger.

- [ ] **Step 2: Write failing planner tests**

Create literal fixtures for two strict tasks and three requests. Assert that
the CLI performs an exact trajectory-request join, promotes only the requested
matched policy key, reprices token categories once, reports every control
attempt separately, and writes deterministic outputs. Add independent tests
that reject duplicate decisions, missing decisions, and an errored strict
request.

- [ ] **Step 3: Run tests and verify RED**

Run:

```bash
python3 -m unittest skills/evaluating-bitrouter-routes/scripts/tests/test_terminal_bench_strong_tier_plan.py -v
```

Expected: FAIL because `terminal_bench_strong_tier_plan.py` does not exist.

- [ ] **Step 4: Implement the minimal planner**

Implement a standard-library CLI that strips ANSI log codes, extracts policy
decision request IDs and matched keys, enforces a one-to-one strict join, sums
the existing nominal costs, reprices only selected requests, computes request
share and per-attempt savings, and writes deterministic JSON/CSV/Markdown plus
a manifest over the other outputs.

- [ ] **Step 5: Run tests and verify GREEN**

Run the same unittest command and require every test to pass without warnings.

- [ ] **Step 6: Pressure-test the updated skill guidance**

Run the same application scenario with the updated skill. Verify that the
operator selects strict evidence, preserves individual controls, treats the
output as non-causal, and does not suggest product-side benchmark logic.

- [ ] **Step 7: Commit the external planner**

```bash
git add skills/evaluating-bitrouter-routes
git commit -m "feat: plan strong tier cost budget"
```

### Task 2: Generate the private optimization artifact

**Files:**
- Create outside the repository worktree: `artifacts/tb21-v1-full1-20260818T075324Z/analysis/strong-tier-optimize-20260819/`

**Interfaces:**
- Consumes: the frozen strict benchmark artifact and the Task 1 planner.
- Produces: a private reproducible evidence bundle; no file is published or included in the source commit.

- [ ] **Step 1: Run the planner over the frozen cohort**

Invoke the script with the frozen validity audit, current request join, daemon
log, all three matched control costs (`16.241240`, `13.117482`, `15.823676`),
target key `agent_route/v1|unknown|mechanical|guarded`, and strong rates
`5,0.5,6.25,30`.

- [ ] **Step 2: Verify exact output invariants**

Assert from `summary.json`: 22 strict tasks, 309 exact requests, zero join
failures, 22 promoted requests, current strong share 82/309, candidate strong
share 104/309, candidate cost approximately $7.105622, and conservative
savings approximately 45.83%.

- [ ] **Step 3: Re-run into a temporary directory**

Compare all output SHA-256 values with the first run and require byte-identical
deterministic output.

### Task 3: Promote the generic template cell

**Files:**
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Modify: `templates/auto-router/policy-lock.yaml`
- Modify: `templates/auto-router/README.md`

**Interfaces:**
- Consumes: the approved target route and Task 2 evidence summary.
- Produces: an official policy template whose guarded-mechanical baseline is strong and whose signed compiler/certificate evidence is internally consistent.

- [ ] **Step 1: Write the failing template behavior test**

Extend the canonical auto-router template test to assert the literal route map
contains `agent_route/v1|unknown|mechanical|guarded: strong`, that the adjacent
mechanical normal/context routes remain economy/balanced, that
`agent_route/v1|unknown|implement|guarded` remains balanced, and that progress
protection remains exactly `{strong, balanced}`.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test -p bitrouter --all-features auto_router_template_routes_have_deterministic_compiler_certificates -- --exact --nocapture
```

Expected: FAIL because the current route is balanced.

- [ ] **Step 3: Change only the selected route**

Set `agent_route/v1|unknown|mechanical|guarded` to strong in the template and
update README prose. Do not alter classifier rules, progress guard, runtime
lookup, model tiers, or any other route.

- [ ] **Step 4: Refresh signed evidence canonically**

Use the existing deterministic template test/helper output to update only the
compiler and route evidence digests required by the selected route change. Do
not hand-author a digest.

- [ ] **Step 5: Run focused tests and config validation**

```bash
cargo test -p bitrouter --all-features auto_router_template -- --nocapture
cargo run -q -p bitrouter --all-features -- config validate --config templates/auto-router/bitrouter.yaml
```

Expected: PASS with the updated route, valid certificates, and unchanged
progress contract.

- [ ] **Step 6: Commit the generic policy change**

```bash
git add apps/bitrouter/src/policy_lock.rs templates/auto-router
git commit -m "feat: raise guarded mechanical quality"
```

### Task 4: Verify, review, and push

**Files:**
- Modify: `.superpowers/sdd/2026-08-19-strong-tier-cost-budget/progress.md`
- Create: `.superpowers/sdd/2026-08-19-strong-tier-cost-budget/final-review.md`

**Interfaces:**
- Consumes: Tasks 1–3 and the private artifact manifest.
- Produces: fresh verification evidence, a clean branch, and a normal push to `origin/codex/next-bitrouter-iteration`.

- [ ] **Step 1: Run focused and skill gates**

```bash
python3 -m unittest skills/evaluating-bitrouter-routes/scripts/tests/test_terminal_bench_strong_tier_plan.py -v
python3 -m unittest skills/evaluating-bitrouter-routes/scripts/tests/test_terminal_bench_route_evidence.py -v
python3 skills/quick_validate.py skills/evaluating-bitrouter-routes
cargo test -p bitrouter --all-features auto_router_template -- --nocapture
```

- [ ] **Step 2: Run full repository gates**

```bash
cargo test --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 3: Audit boundaries and requirements**

Confirm no Terminal-Bench identifier or parser entered Rust runtime code,
non-task errors remain excluded, only the approved route changed, controls are
not averaged, the private artifact is not staged, and the worktree contains no
unrelated changes.

- [ ] **Step 4: Write final review and commit**

Record commands, exit codes, exact evidence hashes, route diff, cost estimate,
limitations, and boundary audit. Commit only the report and ledger.

- [ ] **Step 5: Push normally**

```bash
git push origin codex/next-bitrouter-iteration
```

Verify local HEAD equals the remote branch HEAD and the worktree is clean.
