# Full89 Targeted Matrix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the Terminal-Bench 2.1 full89 matrix with Sol through `r3`, every other model combination through `r2`, independent Claude-model quota handling, and auditable security-policy skips.

**Architecture:** Keep the existing append-only EC2/Harbor runtime and add three narrow contracts: per-combination target groups, model-keyed provider-gate evidence, and immutable per-case skip evidence. Existing strict request joins and cleanup gates remain unchanged for all real executions; group coverage becomes accepted TrialResults plus validated skips.

**Tech Stack:** Python 3 standard library and `unittest`, Harbor/Terminus 2, BitRouter Rust CLI, AWS CLI with profile `benchmark-202607`, JSON/JSONL evidence, tmux on the central EC2 host.

## Global Constraints

- Terminal-Bench version is exactly 2.1 with the frozen 89-task set.
- Harness is Harbor plus Terminus 2; topology is EC2 central BitRouter daemon plus ephemeral EC2 sandboxes.
- AWS access always uses profile `benchmark-202607` and region `us-east-2`; credentials are never written to artifacts or output.
- Case concurrency remains exactly four for every new group and recovery wave.
- Sol targets `control`, `r1`, `r2`, `r3`; Terra, Fable, Sonnet, and Opus target `control`, `r1`, `r2`.
- A Claude `429` applies only to the exact model that returned it and never blocks another Claude model.
- A security skip requires two matching typed refusals and never applies to `429`, `5xx`, timeout, missing usage, or an attempt with a valid TrialResult.
- Official quality uses denominator 89; runnable-only quality is secondary and explicitly labeled.
- Never rerun an accepted group or an accepted case identity.

---

### Task 1: Per-combination target groups and lineage completion

**Files:**
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/drivers/full_matrix_driver.py`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/drivers/full_matrix_runtime.py`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/tests/test_full_matrix_driver.py`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/tests/test_full_matrix_runtime.py`

**Interfaces:**
- Produces: `Combination.target_groups: tuple[str, ...]`
- Produces: `next_group(markers: set[str], target_groups: Sequence[str]) -> Optional[str]`
- Produces: `lineage_is_complete(markers: set[str], target_groups: Sequence[str]) -> bool`
- Consumes: existing `GROUPS`, acceptance markers, and feedback markers.

- [ ] **Step 1: Write failing target-matrix tests**

```python
def test_only_sol_targets_r3(self):
    self.assertEqual(driver.COMBINATIONS["sol"].target_groups, driver.GROUPS)
    for key in ("terra", "fable", "sonnet", "opus"):
        self.assertEqual(driver.COMBINATIONS[key].target_groups, ("control", "r1", "r2"))

def test_three_group_lineage_completes_only_after_r2_feedback(self):
    groups = ("control", "r1", "r2")
    markers = {"CONTROL_ACCEPTED", "R1_ACCEPTED", "R1_FEEDBACK_APPLIED", "R2_ACCEPTED"}
    self.assertFalse(runtime.lineage_is_complete(markers, groups))
    markers.add("R2_FEEDBACK_APPLIED")
    self.assertTrue(runtime.lineage_is_complete(markers, groups))
    self.assertIsNone(runtime.next_group(markers, groups))
```

- [ ] **Step 2: Run the exact tests and verify red**

Run:

```bash
python3 -m unittest tests.test_full_matrix_driver tests.test_full_matrix_runtime -v
```

Expected: failures because `target_groups`, the second `next_group` parameter, and `lineage_is_complete` do not exist.

- [ ] **Step 3: Add target groups and use them for new preparation**

```python
THREE_GROUP_TARGETS = ("control", "r1", "r2")

@dataclass(frozen=True)
class Combination:
    key: str
    strong_target: str
    provider: str
    model: str
    entry_model: str
    protocol: str
    port: int
    capacity: int
    strong_price: str
    provider_family: str
    target_groups: tuple[str, ...]

# Append target_groups=GROUPS to the existing Sol constructor.
# Append target_groups=THREE_GROUP_TARGETS to each existing Terra, Fable,
# Sonnet, and Opus constructor. All other constructor values remain byte-for-byte
# unchanged so provider, price, port, and model identities cannot drift.
```

Change `prepare_combination` and its metadata to iterate over `spec.target_groups`. Existing prepared roots may retain unused `r3` files; validation and execution ignore them.

- [ ] **Step 4: Make runtime validation and lineage advancement target-aware**

```python
def lineage_is_complete(markers, target_groups):
    required = {
        "control": {"CONTROL_ACCEPTED"},
        "r1": {"R1_ACCEPTED", "R1_FEEDBACK_APPLIED"},
        "r2": {"R2_ACCEPTED", "R2_FEEDBACK_APPLIED"},
        "r3": {"R3_ACCEPTED"},
    }
    return all(required[group] <= markers for group in target_groups)
```

Update `validate_inputs`, `next_group`, and `run_lineage` to use `spec.target_groups`. Reject `run_group(run_root, spec, group)` when `group not in spec.target_groups` before starting a daemon. `LINEAGE_ACCEPTED` records exact target groups and SHA-256 hashes of their acceptance/feedback markers.

- [ ] **Step 5: Run full driver/runtime tests and verify green**

Run:

```bash
python3 -m unittest discover -s tests -p 'test_full*.py' -v
```

Expected: all tests pass; existing Sol four-group tests remain green.

- [ ] **Step 6: Commit the target-group implementation**

The runtime directory is not a Git worktree. Record its full file hashes in `HOTFIX-PROVENANCE.md`; commit only repository-backed documentation and skill changes in later tasks.

### Task 2: Model-keyed provider-gate evidence and Claude rotation

**Files:**
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/drivers/full_matrix_runtime.py`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/tests/test_full_matrix_runtime.py`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/FULL-RUN-PLAN.md`

**Interfaces:**
- Produces: `provider_gate_key(spec: Combination) -> str`
- Produces: `summarize_provider_gate(run_root: Path, spec: Combination, group: str) -> dict`
- Produces: CLI `record-provider-gate --combo KEY --run-root PATH --group GROUP`
- Consumes: `controller-state/<group>/pre-batch-gates.jsonl` and strong sentinel trace outcomes; it never reads or copies `raw_body`.

- [ ] **Step 1: Write failing sanitization and model-isolation tests**

```python
def test_provider_gate_summary_is_model_keyed_and_omits_raw_body(self):
    # fixture: pre-batch provider strong=false and sentinel outcome http_status=429
    summary = runtime.summarize_provider_gate(root, driver.COMBINATIONS["fable"], "control")
    self.assertEqual(summary["outcome"], "rate_limited")
    self.assertEqual(summary["model"], "claude-fable-5")
    self.assertNotIn("raw_body", json.dumps(summary))

def test_fable_rate_limit_does_not_create_family_gate(self):
    self.assertNotEqual(
        runtime.provider_gate_key(driver.COMBINATIONS["fable"]),
        runtime.provider_gate_key(driver.COMBINATIONS["sonnet"]),
    )
```

- [ ] **Step 2: Run the two tests and verify red**

Run:

```bash
python3 -m unittest \
  tests.test_full_matrix_runtime.RuntimePureContractTests.test_provider_gate_summary_is_model_keyed_and_omits_raw_body \
  tests.test_full_matrix_runtime.RuntimePureContractTests.test_fable_rate_limit_does_not_create_family_gate -v
```

Expected: functions are undefined.

- [ ] **Step 3: Implement a read-only, sanitized summary**

Read only `recorded_at`, `providers`, `passed`, `reasons`, and `live_sandboxes` from the last gate snapshot. From the matching `*-sentinel-strong` trace read only `headers.x-bitrouter-benchmark-run-id` and `outcome.http_status/status`. Classify `429` as `rate_limited`, typed `403` as `policy_refused`, `2xx` as `ready`, and all other statuses as `transient_error`.

Append the summary with mode `0600` under `evidence/provider-gates/<combo>.jsonl`. The key is `spec.strong_target`; no provider-family marker or cooldown file is created.

- [ ] **Step 4: Add the CLI and run full tests**

Run:

```bash
python3 -m unittest discover -s tests -p 'test_full*.py' -v
```

Expected: all tests pass, including fixtures proving no raw body or credential field is copied.

- [ ] **Step 5: Exercise the existing Fable 429 artifact without launching EC2**

Run the CLI on the existing Fable v3 control root. Expected output artifact: `outcome=rate_limited`, `model=claude-fable-5`, `live_sandboxes=0`. Then run independent zero-sandbox canaries for Sonnet and Opus; a Fable result never suppresses either canary.

### Task 3: Immutable security-policy skips and dual scoring

**Files:**
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/drivers/full_matrix_driver.py`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/drivers/full_matrix_runtime.py`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/tests/test_full_matrix_driver.py`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/tests/test_full_matrix_runtime.py`

**Interfaces:**
- Produces: `load_security_skips(run_root: Path, group: str) -> dict[str, dict]`
- Produces: `validate_security_skip(record: Mapping[str, object], events: Sequence[Mapping[str, object]]) -> None`
- Produces: CLI `record-security-skip` using two attempt identities, one normalized class, and two evidence paths.
- Changes: `recovery_plan(events, case_tasks, evidence_invalidated_case_ids, skipped_case_ids)`, `case_state_summary(events, expected_ids, evidence_invalidated_case_ids, skipped_case_ids)`, and group acceptance coverage.

- [ ] **Step 1: Write failing conservative skip tests**

```python
def test_skip_requires_two_matching_typed_refusals(self):
    events = [
        {"case_id": "case-001-a", "state": "started"},
        {"case_id": "case-001-a", "state": "terminal_invalid"},
    ]
    record = {
        "case_id": "case-001-a",
        "refusal_class": "provider_policy_refusal",
        "attempt_ids": ["case-001-a"],
    }
    with self.assertRaisesRegex(RuntimeError, "two matching"):
        runtime.validate_security_skip(record, events)

def test_transient_statuses_are_never_skippable(self):
    record = {
        "case_id": "case-001-a",
        "attempt_ids": ["case-001-a", "case-001-a-replacement-01"],
    }
    events = [
        {"case_id": attempt, "state": state}
        for attempt in record["attempt_ids"]
        for state in ("started", "terminal_invalid")
    ]
    for reason in ("rate_limited", "upstream_5xx", "timeout", "missing_usage"):
        candidate = dict(record, refusal_class=reason)
        with self.assertRaisesRegex(RuntimeError, "not skippable"):
            runtime.validate_security_skip(candidate, events)

def test_valid_result_prevents_skip(self):
    record = {
        "case_id": "case-001-a",
        "refusal_class": "network_security_policy_denied",
        "attempt_ids": ["case-001-a", "case-001-a-replacement-01"],
    }
    events = [
        {"case_id": "case-001-a", "state": "started"},
        {"case_id": "case-001-a", "state": "terminal_valid"},
        {"case_id": "case-001-a-replacement-01", "state": "started"},
        {"case_id": "case-001-a-replacement-01", "state": "terminal_invalid"},
    ]
    with self.assertRaisesRegex(RuntimeError, "valid TrialResult"):
        runtime.validate_security_skip(record, events)
```

- [ ] **Step 2: Verify the tests fail, then implement exclusive skip records**

The CLI verifies both evidence files exist, hashes them, confirms two distinct terminal-invalid attempt identities for one original case, accepts only `provider_policy_refusal` or `network_security_policy_denied`, and writes exactly once to `case-skips/<group>/<case-id>.json`. It stores paths/hashes and never copies evidence contents.

- [ ] **Step 3: Exclude valid skips from recovery while preserving all other failures**

Pass `skipped_case_ids` into `recovery_plan`. A skipped case is omitted; a `429`, timeout, `5xx`, missing settlement, or untyped invalid case remains in the recovery plan. `case_state_summary` requires exactly one valid attempt for non-skipped cases and exactly the two evidenced terminal-invalid attempts for skipped cases.

- [ ] **Step 4: Make acceptance cover 89 without synthetic rows**

Set `expected_task_count=89`, `accepted_task_count=len(selected)`, and `security_skipped_task_count=len(skips)`. Require their sum to equal 89. Reward join `outcome_count` equals accepted task count, not 89. Bundle inputs contain only real selected attempts.

Emit:

```python
official_quality = reward_sum / 89
runnable_quality = reward_sum / accepted_task_count if accepted_task_count else 0.0
```

No skipped case receives a fabricated TrialResult, request, usage, outcome, or reward.

- [ ] **Step 5: Run all tests**

Run:

```bash
python3 -m unittest discover -s tests -p 'test_full*.py' -v
```

Expected: all tests pass; the no-skip 89/89 acceptance fixture remains unchanged.

### Task 4: Deploy target-aware runtime without disturbing Sol r3

**Files:**
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/HOTFIX-PROVENANCE.md`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/RUN-STATUS.md`

**Interfaces:**
- Consumes: local tested driver/runtime hashes.
- Produces: identical central files and a zero-launch validation record.

- [ ] **Step 1: Wait for the active Sol r3 controller/tmux to finish before replacing central runtime files**

Do not hot-swap the files used by PID-owning Sol processes. Continue read-only event, trace-growth, IAM, and EC2 residue monitoring.

- [ ] **Step 2: Verify and accept Sol r3**

Require 89 selected TrialResults, exact five-bucket usage, strict cost/reward/session joins, peak four, tail zero, and instance/EBS/ENI `0/0/0`. If only exact cases fail, create immutable replacements for those cases only. Write `R3_ACCEPTED` and `LINEAGE_ACCEPTED` only after independent verification.

- [ ] **Step 3: Copy tested runtime files to the central host and verify hashes**

Use the approved SSH path and `benchmark-202607` IAM profile. Record local and central SHA-256 values in `HOTFIX-PROVENANCE.md`. Run central driver/controller tests and zero-sandbox target-boundary checks for Terra, Fable, Sonnet, and Opus.

- [ ] **Step 4: Send the authorized Sol result email**

Send only the control/r1/r2/r3 key metrics table to `kelsen@bitrouter.ai`: score/pass count, total cost and savings, strong/weak requests, five token buckets, join status, sandbox peak/tail, and cleanup. Do not attach full traces or secrets.

### Task 5: Run the four remaining three-group lineages

**Files:**
- Modify continuously: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/RUN-STATUS.md`
- Produce on central EC2: immutable run roots and accepted artifacts for Terra, Fable, Sonnet, and Opus.

**Interfaces:**
- Consumes: `run-lineage`, `recover-group`, provider-gate evidence, skip evidence, and frozen capacity four.
- Produces: `CONTROL_ACCEPTED`, `R1_ACCEPTED`, `R1_FEEDBACK_APPLIED`, `R2_ACCEPTED`, `R2_FEEDBACK_APPLIED`, and `LINEAGE_ACCEPTED` per combination.

- [ ] **Step 1: Finish or recover Fable control's existing 75/89 progress**

Probe Fable only when its own model gate is due. If Fable is `429`, record it with zero new case identity and move to Sonnet or Opus rather than declaring a Claude-family cooldown.

- [ ] **Step 2: Run Terra control, r1, and r2 serially**

Use exact-case recovery and strict postprocess after each group. Stop after `R2_FEEDBACK_APPLIED`; verify no r3 daemon or case identity exists.

- [ ] **Step 3: Run Fable, Sonnet, and Opus through r2 as their independent model gates allow**

Never substitute one model's outputs into another lineage. For stable per-case security refusals, allow one confirmation replacement and then use the validated skip ledger; keep official denominator 89.

- [ ] **Step 4: Verify every lineage independently**

For each run root, inspect target markers, TrialResults plus skips, five token buckets, strict joins, feedback hashes, bundle hashes, model routing counts, monitor peak/tail, and exact AWS cleanup.

### Task 6: Final documentation, skill, PR, and matrix audit

**Files:**
- Modify: `skills/run-bitrouter-benchmark/SKILL.md`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/FULL-RUN-PLAN.md`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/RUN-STATUS.md`
- Modify: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-full89-matrix-20260720/HOTFIX-PROVENANCE.md`

**Interfaces:**
- Produces: public QnA for target groups, per-model `429`, typed security skips, dual denominators, recovery, IAM, and cleanup.
- Produces: final matrix table and PR #717 update.

- [ ] **Step 1: Update the pure-document benchmark skill**

Add QnA entries explaining that Claude quotas are model-specific, how to switch lineages without substituting results, the two-attempt skip rule, official/runnable scoring, and why monitoring failures never restart tmux.

- [ ] **Step 2: Run skill and repository verification**

Run the repository's skill validator and the `bitrouter-skills` test suite. Expected: all tests pass and no secret-like values appear in the changed files.

- [ ] **Step 3: Perform the completion audit**

Require one Sol four-group lineage and four three-group lineages, all target markers, all 89-task coverage, exact joins, all cleanup counts zero, final hashes, and no unintended r3 artifacts consumed for non-Sol combinations.

- [ ] **Step 4: Commit and push reviewed repository changes**

Commit only intentional source/spec/skill changes, push the actual PR #717 head branch, and verify GitHub checks. Remove the exact temporary bastion security-group rule after all remote monitoring is complete.
