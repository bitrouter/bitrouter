# Generic Agentic Eval and short13 Implementation Plan

> Execute this plan in order. Do not start a scored trial until the source,
> models, manifests, prices, evaluator, and initial lock are frozen.

**Goal:** Ship a generic BitRouter agentic-eval skill, repair the eval baseline
and pretrained-route evolution seams it exposes, then produce an accepted
control+r1+r2+r3 Terminal-Bench short13 lineage.

**Architecture:** BitRouter remains a lock-only serving router. Evaluators run
outside the request path and submit immutable results to the Eval Exchange.
Each accepted round freezes a content-addressed snapshot, compiles a separate
candidate, and explicitly publishes the exact candidate for the next round.

**Stack:** Rust, Tokio, Clap, YAML/JSON Eval Exchange, Agent Skills, Harbor,
Terminus-2, Terminal-Bench 2.1, AWS EC2, Codex subscription provider, BitRouter
Cloud DeepSeek V4 Pro.

---

## Task 1: Preserve design and baseline evidence

**Files:**

- Add: `docs/superpowers/specs/2026-07-31-agentic-eval-short13-design.md`
- Add: `docs/superpowers/plans/2026-07-31-agentic-eval-short13.md`

1. Record the selected product boundary, alternative designs, frozen task set,
   group sequence, acceptance rules, and cost semantics.
2. Record the current clean baseline:
   `cargo build -p bitrouter` and the two eval control-plane integration tests.
3. Commit the design and plan before implementation.

## Task 2: RED-test the generic skill

**Files:**

- Modify: the design spec with a concise baseline-failure table

1. Dispatch independent agents without the new skill against three raw cases:
   multi-turn task eval, human feedback, and private enterprise eval.
2. Capture exact invalid commands, invented fields, unsafe evidence handling,
   attribution errors, or evaluator/router boundary violations.
3. Classify whether each failure needs a positive output contract, a conditional
   rule, or a hard prohibition. Do not write the skill first.

## Task 3: Create the generic agentic-eval skill

**Files:**

- Modify: `apps/bitrouter/src/main.rs`
- Modify: `skills/bitrouter/references/cli.md`
- Test: `apps/bitrouter/src/main.rs`
- Add: `skills/evaluating-bitrouter-routes/SKILL.md`
- Add: `skills/evaluating-bitrouter-routes/references/eval-exchange.md`
- Add: `skills/evaluating-bitrouter-routes/agents/openai.yaml`
- Modify: `skills/README.md`

1. Add a failing CLI test for `bitrouter eval subject seal <draft> --output
   <sealed>`. The command must calculate the canonical evidence digest, validate
   the completed subject, write deterministic JSON, and never touch the ledger.
2. Implement the minimal command and document it in the core CLI reference.
3. Initialize the skill with the repository as its destination and generate UI
   metadata using the skill-creator script.
4. Write a short verb-led skill that selects request/episode/task scope, builds
   an evidence-bound result, submits it, verifies admission, and stops before
   policy publication.
5. Put exact wire fields, metric units, authority configuration, CLI/REST
   commands, ownership behavior, and failure semantics in one reference.
6. Include one complete generic multi-decision example. Do not mention
   Terminal-Bench in the skill contract.
7. Run `quick_validate.py`, `skills-ref validate`, and word/line checks.
8. Re-run the three application scenarios with the skill and close any gaps.

## Task 4: Fix certificate baseline propagation with TDD

**Files:**

- Modify: `apps/bitrouter/src/policy_table_router.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Test: `apps/bitrouter/src/policy_table_router.rs`

1. Add a failing router test in which an explicit economy route has strong as
   its eval baseline. Verify the pending eval decision currently reports the
   selected economy tier instead of strong.
2. Add a second failing case where the certificate baseline differs from the
   policy default.
3. Pass immutable per-route certificate baselines and a default baseline into
   the eval observer created by `PolicyRuntime`.
4. Use the resolved baseline for both `PendingEvalDecision` and
   `PolicyDecisionRecord`; keep selected/static tier as actual routing state.
5. Run the focused router, settlement, and policy-lock tests.

## Task 5: Make pretrained routes evolvable with TDD

**Files:**

- Modify: `templates/auto-router/policy-lock.yaml`
- Modify: `templates/auto-router/README.md`
- Test: `apps/bitrouter/src/policy_lock.rs` or
  `apps/bitrouter/src/policy_compile.rs`

1. Add a failing test that loads the shipped auto-router template and asserts
   that migrated learned routes are compiler-owned and have a consistent
   non-operator source.
2. Change only the three pretrained route certificates from pinned operator
   ownership to compiler ownership with truthful migrated source metadata.
3. Add a focused compile test showing admitted negative evidence can demote a
   pretrained economy route instead of creating a blocking conflict.
4. Document that operator-authored routes remain pinned and runtime mode still
   owns publication authority.
5. Validate the template and run policy compile/publish tests.

## Task 6: Freeze the operational lineage

**Operational artifacts:** create outside tracked source under a fresh,
append-only run ID on the benchmark controller.

1. Freeze the 13 task IDs from the design, one trial each, retries zero.
2. Freeze current BitRouter commit, Harbor and Terminal-Bench revisions,
   Terminus-2 config, AWS identity/region/quota/type/tags, concurrency, ports,
   provider/model/protocol IDs, evaluator identity/config digest, and prices.
3. Define four fresh groups: matching strong-only control, `r1`, `r2`, `r3`.
4. Use one fresh policy database/ledger for the policy lineage. Give each group
   a fixed lock digest; no active bytes change during a group.
5. Mark the run prepared only after immutable manifest validation succeeds.

## Task 7: Configure credentials and run canaries

1. Start the existing stopped central benchmark EC2 controller and record its
   exact instance/volume/network identity.
2. Import the local Codex subscription credential into the central BitRouter
   credential store without logging or copying it into manifests.
3. Store the supplied BitRouter Cloud credential through `bitrouter cloud
   login`; never place it in argv captures, config, shell history, or artifacts.
4. Create a scoped BitRouter virtual key for sandboxes.
5. Validate config and policy, then run non-evaluation direct strong and direct
   economy sentinels. Require settlement and reconciliation.
6. Run a fresh real Terminus-2 ephemeral-EC2 canary. Prove artifact collection,
   request joins, provider settlement, and deletion before scoring.

## Task 8: Run control and r1

1. Run all 13 control trials against explicit
   `openai-codex:gpt-5.6-sol` with the frozen controller and concurrency.
2. Accept or reject the group as a whole. Never reuse a consumed case identity.
3. Run all 13 `r1` trials against `@auto:cost` and the pretrained lock.
4. Accept or reject `r1` using the same strict gate.
5. Assemble one redacted task eval packet per accepted trial and use the new
   skill to submit task-native and agentic results with explicit decision credit.
6. Freeze the admitted snapshot, compile `r2` candidate, inspect the diff and
   certificates, then publish the exact candidate under adaptive mode.

## Task 9: Run r2 and r3 evolution

1. Run and accept all 13 `r2` trials under the fixed `r2` lock.
2. Evaluate only accepted artifacts, freeze a new cumulative snapshot, compile,
   diff, and explicitly publish the exact `r3` candidate.
3. Run and accept all 13 `r3` trials under the fixed `r3` lock.
4. Freeze a final audit snapshot without publishing an unmeasured `r4` policy.
5. Verify active lock evidence and record all parent/child digests.

## Task 10: Compile results and clean resources

1. Produce per-task outcomes and group pass counts/rates.
2. Produce strong/economy request and token mix, actual Cloud spend,
   subscription/notional cost separately, EC2 cost, evaluator cost, and totals
   with unknown components explicit.
3. Explain every `r1 -> r2 -> r3` route/certificate change from admitted evidence.
4. Query exact run tags for instances, volumes, and interfaces; remove leaks and
   stop the central controller after artifacts are durable.
5. Scan tracked and operational artifacts for credential-shaped material.

## Task 11: Verify and publish the PR

1. Run focused tests after each RED/GREEN change.
2. Run `cargo fmt -- --check` and `cargo clippy --all-features`.
3. Run `cargo nextest run --all-features` when available, otherwise
   `cargo test --all-features`.
4. Review the diff for scope, secrets, ignored artifacts, and accidental
   benchmark-specific coupling in the generic skill.
5. Request an independent code review and address actionable findings.
6. Commit conventional, scoped changes; push the `codex/` branch; open a
   non-draft merge-ready PR; monitor required checks to completion.
