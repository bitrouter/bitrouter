# Finalization Exploration Guardrail Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent online exploration from downgrading finalization requests, then build and run one fully accepted policy-only Terminal-Bench short13 r1-r3 lineage.

**Architecture:** `PolicyTable` computes one exploration-eligibility value from the normalized `OnlineWorkflowState`; both request routing and the adequacy observer consume it. Finalization is ineligible, opening retains its existing opt-in rule, and static routes remain operator-owned. The benchmark freezes the resulting Linux binary and runs r1, reward feedback, r2, reward feedback, and r3 against the immutable control artifact with concurrency three.

**Tech Stack:** Rust, Tokio, existing BitRouter policy and Workflow State IR modules, Python 3 controller/driver tests, Harbor Terminus 2, AWS EC2, AWS CLI with explicit IAM profile `benchmark-202607`.

## Global Constraints

- Preserve the user-owned deletions at `dist/registry/models.json`, `dist/registry/providers.json`, and `dist/schema/bitrouter.config.schema.json`.
- Do not add production `#[allow]`, `unwrap`, `expect`, or `panic`.
- Keep OSS code and comments in English.
- Do not expose AWS, OpenAI OAuth, provider API, or BitRouter API credentials in logs or artifacts.
- Every AWS CLI call must include `--profile benchmark-202607`; `aws login`, SSO, ambient default credentials, and instance-role fallback are forbidden.
- Reuse control artifact `sha256:59be778ac782dd2cc1e4b914718af2b9358b2eb92da9a3d8675040b414d5e540`; never execute a control case.
- Start a fresh policy database and run only r1, r2, and r3 with fixed concurrency three.
- A transient upstream timeout may receive one case retry only under the eligibility and attempt-accounting rules in the approved design; no other automatic retry is allowed.
- Do not accept a round without 13 valid outcomes, authoritative four-bucket settlement, exact cost/reward/session joins, and zero residual sandbox instances.

---

### Task 1: Shared finalization exploration eligibility

**Files:**
- Modify: `apps/bitrouter/src/policy_table_router.rs`
- Test: `apps/bitrouter/src/policy_table_router.rs`
- Test: `apps/bitrouter/src/adequacy/observer.rs`

**Interfaces:**
- Consumes: `OnlineWorkflowState`, `WorkflowStateKind`, `HarnessId`, and `AgentRole`.
- Produces: one `PolicyTable::exploration_allowed_for_online(&OnlineWorkflowState) -> bool` decision used by router and observer, plus a recorded per-request eligibility bit in `PolicyDecision`.

- [ ] **Step 1: Add failing router tests for due trial, learned lock, and static cheap finalization**

Add a helper that produces a Terminus 2 finalization prompt and explicit main-agent headers:

```rust
fn terminus_finalization() -> Vec<Message> {
    vec![
        user("finish the task"),
        assistant_text(r#"{"commands":[],"task_complete":true}"#),
    ]
}

fn terminus_main_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-bitrouter-harness",
        HeaderValue::from_static("terminus_2"),
    );
    headers.insert(
        "x-bitrouter-agent-role",
        HeaderValue::from_static("main"),
    );
    headers
}
```

Use a cadence-due ledger to assert finalization stays `vendor/flagship`, with `StaticTable`, `trialed == false`, even though the corresponding ordinary candidate would trial cheap. Use a threshold-one successful exploration outcome to create a learned lock and assert finalization still stays flagship. Build a workflow-state table with its exact finalization routing key mapped to cheap and assert that explicit static route still selects `vendor/cheap`.

- [ ] **Step 2: Run the router tests and verify RED**

```bash
cargo test -p bitrouter policy_table_router::tests::finalization -- --nocapture
```

Expected: the due-trial and learned-lock assertions fail because finalization currently enters the ordinary exploration branch. The static-cheap assertion passes and protects operator ownership.

- [ ] **Step 3: Add a failing observer test**

In `adequacy/observer.rs`, serve the same Terminus 2 finalization prompt on `vendor/cheap` with a failed request outcome, using `explicit_route_workflow_explore_table()`. Assert the ledger is not pinned for the computed workflow routing key:

```rust
let messages = terminus_finalization();
let headers = terminus_main_headers();
let online = OnlineWorkflowState::from_headers(&headers, &prompt(messages.clone()));
let key = online.routing_key().to_string();
let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 1, 1));
let hook = AdequacyObserveHook::new(explicit_route_workflow_explore_table(), ledger.clone());
hook.on_request_end(
    &ctx_with_headers("bitrouter:moonshotai/kimi-k2.7-code", messages, headers),
    &failed(),
).await;
assert!(!ledger.is_pinned(&key));
```

- [ ] **Step 4: Run the observer test and verify RED**

```bash
cargo test -p bitrouter adequacy::observer::tests::finalization -- --nocapture
```

Expected: FAIL because the observer currently treats the served cheap finalization request as a failed exploration trial and pins it.

- [ ] **Step 5: Implement the minimal shared eligibility rule**

Add a private `exploration_allowed: bool` field to `PolicyDecision`. Compute it from `OnlineWorkflowState` before the decision struct is built. Replace the duplicated prompt and decision checks with one table method:

```rust
fn exploration_allowed_for_online(&self, online: &OnlineWorkflowState) -> bool {
    if online.ir.harness_id == HarnessId::Terminus2
        && online.ir.identity.role == AgentRole::Unknown
    {
        return false;
    }
    match online.ir.state_kind {
        WorkflowStateKind::Finalization => false,
        WorkflowStateKind::Opening => self.can_explore_opening(),
        _ if online.legacy_fingerprint() == "opening" => self.can_explore_opening(),
        _ => true,
    }
}
```

`PolicyTable::exploration_allowed_for_prompt` reconstructs `OnlineWorkflowState` and delegates to this method. `PolicyTableRouter::exploration_allowed_for` returns the stored decision bit. Do not change static-tier resolution, tool guards, pins, or reliability permits.

- [ ] **Step 6: Run focused tests and verify GREEN**

```bash
cargo test -p bitrouter policy_table_router::tests::finalization -- --nocapture
cargo test -p bitrouter adequacy::observer::tests::finalization -- --nocapture
cargo test -p bitrouter policy_table_router::tests::opening -- --nocapture
cargo test -p bitrouter policy_table_router::tests::exploration -- --nocapture
```

Expected: all selected tests pass; finalization never records an exploration trial or lock, static cheap remains cheap, and opening/ordinary exploration behavior is unchanged.

- [ ] **Step 7: Commit the independently testable behavior change**

```bash
git add apps/bitrouter/src/policy_table_router.rs apps/bitrouter/src/adequacy/observer.rs
git commit -m "fix(policy): protect finalization from exploration"
```

### Task 2: Full repository verification and Linux release pin

**Files:**
- Modify only files required by formatter, compiler, or test feedback.
- Modify after successful Linux build: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/benchmark-runtime-repair-20260716/drivers/run_short13_policy_lineage.py`
- Test: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/benchmark-runtime-repair-20260716/drivers/test_run_short13_policy_lineage.py`

**Interfaces:**
- Consumes: Task 1 commit and existing central EC2 build environment.
- Produces: one pushed OSS commit, a Linux release SHA-256, and a driver whose immutable `OSS_COMMIT` and `EXPECTED_BINARY_SHA256` pins match that build.

- [ ] **Step 1: Run all local verification**

```bash
cargo fmt --all -- --check
cargo test -p bitrouter --lib
cargo test -p bitrouter --test workflow_artifact_integration
cargo test -p bitrouter-sdk config
cargo clippy -p bitrouter --all-targets -- -D warnings
```

Expected: every command exits zero with no failed test and no warning.

- [ ] **Step 2: Verify the worktree scope and push the branch**

```bash
git status --short
git log -3 --oneline
git push origin codex/c0-c1-policy-router
```

Expected: only the three preserved user deletions remain unstaged; the design and implementation commits are on the branch; push succeeds and updates PR #717.

- [ ] **Step 3: Build the exact commit on the central Linux host**

Use the existing bastion and central host, fetch the pushed branch, detach at the exact implementation commit, and run:

```bash
cargo build --release -p bitrouter
sha256sum target/release/bitrouter
```

Expected: release build exits zero. Record the commit and SHA-256 without printing any credential.

- [ ] **Step 4: Add failing driver pin tests, then update immutable pins**

Add assertions that the current OSS commit differs from the rejected lineage commit, the expected binary SHA is 64 lowercase hexadecimal characters, `CANARY_OSS_COMMIT` remains `9d3b94115d45622ab61c32d70932cde31b34907c`, and prepared metadata carries the new exact values. Run:

```bash
python3 /Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/benchmark-runtime-repair-20260716/drivers/test_run_short13_policy_lineage.py
```

Expected RED before updating constants, then all tests pass after setting `OSS_COMMIT` and `EXPECTED_BINARY_SHA256` to the Linux build values.

- [ ] **Step 5: Synchronize and verify the driver on the central host**

Copy only the updated driver and its test to `/home/ubuntu/bitrouter-bench/runtime-repair/drivers`, then run its full unit suite with the central Python environment.

Expected: all driver tests pass, the canary pin is unchanged, and no secret value appears in output.

### Task 3: Freeze a fresh policy-only short13 lineage

**Files:**
- Create remotely under `/home/ubuntu/bitrouter-bench/runtime-repair/runs/terminus2-terra-c0c1-short13-policy-finalization-guard-20260718T170000Z/` via the driver.
- Do not modify the rejected lineage or immutable control run.

**Interfaces:**
- Consumes: the exact Linux binary and updated driver pins from Task 2.
- Produces: a fresh immutable manifest, fresh policy database, three round configs, control reference, pricing snapshot, and zero control launches.

- [ ] **Step 1: Verify AWS and central runtime preconditions**

Run quota and EC2 probes with explicit `--profile benchmark-202607`, verify the central daemon host and bastion are reachable, verify the binary SHA and detached commit, and verify no residual sandboxes for either the rejected or proposed run ID.

Expected: quota supports concurrency three, runtime pins match, IAM identity succeeds, and both residual-instance queries return empty arrays.

- [ ] **Step 2: Prepare a timestamped new run**

```bash
python3 /home/ubuntu/bitrouter-bench/runtime-repair/drivers/run_short13_policy_lineage.py \
  prepare --run-id terminus2-terra-c0c1-short13-policy-finalization-guard-20260718T170000Z
```

Expected: `PREPARED` contains the exact OSS commit; `CONTROL_REFERENCED` contains the immutable artifact ID; metadata says `control_launches: 0`, `max_parallel_sandboxes: 3`, and a fresh shared policy database path.

- [ ] **Step 3: Audit frozen inputs before launch**

Validate all 39 Harbor configs, all three BitRouter configs, manifest hashes, Terminus 2 + `openai/gpt-5.6-terra`, explicit EC2 ephemeral deletion tags, pricing snapshot including four token buckets, and absence of Claude models or control launch entries.

Expected: every frozen-input check passes before the first sandbox is launched.

### Task 4: Execute r1, r2, and r3 with strict gates

**Files:**
- Populate the new remote run root with controller state, Harbor results, traces, decisions, usage, outcomes, artifacts, feedback, monitor, and cleanup evidence.

**Interfaces:**
- Consumes: Task 3 frozen lineage.
- Produces: `R1_ACCEPTED`, `R1_FEEDBACK_APPLIED`, `R2_ACCEPTED`, `R2_FEEDBACK_APPLIED`, and `R3_ACCEPTED` with strict artifacts.

- [ ] **Step 1: Run and accept r1**

Execute `run-round --round r1`. Verify 13 valid TrialResults, controller terminal-valid, peak concurrency at most three, tail zero, authoritative four-bucket settlement, no unknown request, exact cost/reward/session joins, and zero residual EC2 sandbox. Only then accept r1.

- [ ] **Step 2: Apply r1 reward feedback exactly once**

Execute `apply-feedback --round r1`. Verify before/after database snapshots, output evidence, one marker, and no duplicate application.

- [ ] **Step 3: Run and accept r2**

Execute `run-round --round r2` and apply the same strict acceptance checks as r1.

- [ ] **Step 4: Apply r2 reward feedback exactly once**

Execute `apply-feedback --round r2` and verify the same idempotency evidence as r1.

- [ ] **Step 5: Run and accept r3**

Execute `run-round --round r3` and apply the same strict acceptance checks. Confirm finalization decisions contain no `exploration_trial` or `exploration_locked` reason.

- [ ] **Step 6: Handle one eligible transient timeout only if encountered**

If a round is runtime-invalid, first prove all five retry-eligibility conditions in the approved design from TrialResult, BitRouter, controller, and cleanup evidence. If eligible, inspect whether the current driver can preserve attempt one, isolate attempt two, include both attempt costs, assign zero reward to failed attempt requests, and retain exact joins. Implement and test the missing attempt-aware path before launching exactly one retry. Otherwise reject the lineage without retry. Never retry a second time.

### Task 5: Completion audit, reports, and PR handoff

**Files:**
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-terra-cc4af6d-short13-20260714T194346Z/RUN-STATUS.md`
- Create: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-terra-c0c1-short13-policy-finalization-guard-20260718T170000Z/SHORT13-RESULT.md`
- Modify only if CLI drift occurred: `skills/bitrouter/`
- Update: GitHub PR #717 body and evidence links.

**Interfaces:**
- Consumes: all strict artifacts and logs from Task 4.
- Produces: a requirement-by-requirement accepted-run report, current status, and a PR whose code, checks, and benchmark evidence agree.

- [ ] **Step 1: Generate the cross-round evidence table**

Report each round's valid cases, verifier reward, request count, strong/cheap split, four-bucket actual cost, notional control cost, cost delta, finalization route reasons, retry count, joins, peak concurrency, and cleanup. Do not claim statistical stability from a single sample.

- [ ] **Step 2: Audit every completion invariant from authoritative files**

Re-read the approved design and this plan. For each goal, non-goal, test, runtime gate, marker, and artifact, cite the file or command output that proves it. Treat missing or indirect evidence as failure and continue work.

- [ ] **Step 3: Update status and PR #717**

Replace stale rejected-lineage status with both historical rejection and the new accepted lineage. Update PR #717 with the exact OSS commit, Linux binary SHA, test counts, short13 artifact IDs, strict joins, settlement, reward/cost summary, and EC2 cleanup evidence. Keep it draft unless all repository checks and runtime gates are green.

- [ ] **Step 4: Run final repository and remote cleanup verification**

Run fresh formatting, test, clippy, branch-status, PR-check, marker, artifact-digest, process, and explicit-profile EC2 queries. Expected: all required checks pass, no unexpected worktree changes exist beyond the preserved deletions, every round is accepted, and no sandbox remains.
