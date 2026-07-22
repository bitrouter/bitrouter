# Terminal-Bench Full-Run Readiness Implementation Plan

> **Claude route update (2026-07-20):** The checked compatibility rejection in
> this historical plan describes the pre-bridge binary. Claude execution now
> follows `2026-07-20-anthropic-ingress-claude-subscription.md` and must pass a
> new immutable EC2 canary before it is accepted.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Merge PR #717 with current main, prove one frozen BitRouter binary can serve every Terminus 2-compatible provider/model route required by a one-trial Terminal-Bench 2.1 full run, and fail closed on incompatible subscription routes before consuming identities or AWS resources.

**Architecture:** Integrate in a clean worktree, compositionally resolve main/PR conflicts, regenerate derived artifacts, and freeze one release binary. Deploy that exact binary to the existing central EC2 host, then run isolated non-scoring provider checks with controller capacity 4 and strict trace, four-bucket settlement, session, TrialResult, and AWS cleanup gates. Terminus 2 compatibility is checked before credentials or resource allocation, so unsupported subscription routes consume no trial identity. Any reusable operational finding is added to the documentation-only `run-bitrouter-benchmark` skill.

**Tech Stack:** Rust/Cargo, BitRouter OpenAI-compatible ingress and provider adapters, Harbor, Terminus 2, Python controller, AWS EC2/Service Quotas/STS CLI, SQLite workflow-state evidence, Markdown Agent Skill.

## Global Constraints

- Benchmark method is Terminal-Bench 2.1 + Harbor + Terminus 2 + one central EC2 daemon + one ephemeral EC2 sandbox per trial.
- Run class is an internal one-trial mechanism run, not a public five-trial reproduction.
- AWS selector is the explicit named access-key profile `benchmark-202607` in `us-east-2`; ambient/default AWS identity is forbidden.
- Terminus 2 must not use the `claude-code` subscription provider because it does not originate the genuine Claude Code agent-profile marker. Claude requires the separate `anthropic` API-key provider or is excluded. Codex uses the protected `openai-codex` OAuth store; Cloud uses the protected BitRouter credential source.
- Provider secrets never enter Harbor configs, command arguments, traces, logs, manifests, Git, or chat.
- Every validation launched after 2026-07-20 has controller capacity 4. A one-case canary still has an observed sandbox peak of one; a multi-case capacity validation must exercise four.
- No scored full-run case starts during this plan.
- A serving binary/config is never replaced inside a canary or scored lineage.
- The original dirty PR #717 worktree is preserved unchanged.

---

### Task 1: Merge latest main into PR #717

**Files:**
- Modify: `apps/bitrouter/src/main.rs`
- Modify: `crates/bitrouter-sdk/src/language_model/protocol/chat_completions.rs`
- Modify: `crates/bitrouter-sdk/src/language_model/settlement.rs`
- Regenerate: `dist/registry/models.json`
- Regenerate: `dist/registry/providers.json`
- Regenerate: `dist/schema/bitrouter.config.schema.json`
- Regenerate when changed by main: `docs/get-started/supported-models.md`
- Regenerate when changed by main: `docs/get-started/supported-models.zh.md`
- Regenerate when changed by main: `docs/get-started/supported-providers.md`
- Regenerate when changed by main: `docs/get-started/supported-providers.zh.md`

**Interfaces:**
- Consumes: `origin/codex/c0-c1-policy-router` at `c20092d8d7b214837ffc0653bb39bd673e1563a6` and `origin/main` at `f6ca31e5bb9279080db2eda302adc734b1c42df8`.
- Produces: one merge commit whose first parent contains PR #717 and second parent is exact current main, with generated artifacts derived from the merged sources.

- [ ] **Step 1: Re-confirm the clean integration state**

Run:

```bash
git status --short --branch
git rev-parse HEAD
git rev-parse origin/main
```

Expected: no working-tree changes; HEAD descends from `c20092d8`; main is `f6ca31e5`.

- [ ] **Step 2: Merge main and capture the expected conflicts**

Run:

```bash
git merge --no-ff origin/main
```

Expected: conflicts only where both branches changed the same CLI test/import list. Stop if Git reports a conflict outside the three files listed above.

- [ ] **Step 3: Preserve both CLI tests in `main.rs`**

Keep both complete functions, in this order, without conflict markers:

```rust
#[test]
fn workflow_state_reliability_report_flags_parse() {
    // Keep the complete PR #717 test body.
}

#[test]
fn cloud_api_owns_header_short_flag_in_full_command_tree() {
    // Keep the complete main-branch test body.
}
```

Do not rewrite either test's assertions.

- [ ] **Step 4: Compose the protocol imports**

In `chat_completions.rs`, retain both main's `ChatStreamOptions` and PR #717's `UsageOrigin` in the existing `types` import. In `settlement.rs`, retain both main's `FinishReason` and PR #717's `UsageOrigin`:

```rust
use crate::language_model::types::{FinishReason, RoutingTarget, UsageOrigin};
```

- [ ] **Step 5: Verify no conflict marker remains**

Run:

```bash
rg -n '^(<<<<<<<|=======|>>>>>>>)' apps crates
```

Expected: no output.

- [ ] **Step 6: Rebuild generated outputs**

Run:

```bash
cargo run -p dist-helper -- registry build
cargo run -p dist-helper -- registry docs
cargo run -p dist-helper -- generate-schema
cargo run -p dist-helper -- check
```

Expected: all commands exit zero and `dist/` matches the merged source catalogs/schema.

- [ ] **Step 7: Complete and commit the merge**

Run:

```bash
git add apps/bitrouter/src/main.rs crates/bitrouter-sdk/src/language_model/protocol/chat_completions.rs crates/bitrouter-sdk/src/language_model/settlement.rs dist docs/get-started
git commit
```

Expected: one merge commit, no unmerged paths, conventional existing merge message retained.

---

### Task 2: Prove merged source and freeze the serving binary

**Files:**
- Test: `apps/bitrouter/src/claude_code.rs`
- Test: `crates/bitrouter-providers/src/claude_code.rs`
- Test: `crates/bitrouter-providers/src/codex/mod.rs`
- Test: `crates/bitrouter-sdk/src/language_model/protocol/tests.rs`
- Test: `apps/bitrouter/src/metering/tests.rs`
- Test: `apps/bitrouter/tests/terminus_2_workflow_state.rs`
- Test: `apps/bitrouter/tests/workflow_state_replay.rs`
- Output only: `target/release/bitrouter`

**Interfaces:**
- Consumes: clean merged source from Task 1.
- Produces: a release binary SHA-256, source commit, Cargo.lock hash, and passing regression record used by every canary.

- [ ] **Step 1: Run focused auth/protocol/metering tests**

Run:

```bash
cargo test -p bitrouter-providers claude_code
cargo test -p bitrouter-providers codex
cargo test -p bitrouter-sdk language_model::protocol
cargo test -p bitrouter --lib metering
cargo test -p bitrouter --test terminus_2_workflow_state
cargo test -p bitrouter --test workflow_state_replay
```

Expected: every selected test passes; in particular the environment-token Claude auto-enable test and cache-aware usage tests execute.

- [ ] **Step 2: Run branch regression gates**

Run:

```bash
cargo fmt --all -- --check
cargo test -p bitrouter --lib
cargo test -p bitrouter-sdk config
cargo clippy -p bitrouter --all-targets -- -D warnings
```

Expected: all exit zero without allowances or warnings.

- [ ] **Step 3: Build the one serving binary**

Run:

```bash
cargo build --release -p bitrouter
git rev-parse HEAD
shasum -a 256 Cargo.lock target/release/bitrouter
```

Expected: one clean source commit and two stable SHA-256 values recorded in the private readiness manifest. Re-running `shasum` without rebuilding returns identical values.

---

### Task 3: Freeze the central-host and provider preflight

**Files:**
- Review: `skills/run-bitrouter-benchmark/references/configuration.md`
- Review: `skills/run-bitrouter-benchmark/references/operations.md`
- Operator-private output: frozen manifest and redacted manifest outside the repository
- Operator-private output: four daemon configs and four direct-sentinel evidence bundles outside the repository

**Interfaces:**
- Consumes: frozen binary/source hashes from Task 2 and protected central-host credential sources.
- Produces: four config-valid direct routes and a central host proven ready for real-agent canaries.

- [ ] **Step 1: Prove AWS identity and quota without ambient fallback**

Run:

```bash
aws --profile benchmark-202607 sts get-caller-identity --output json
aws --profile benchmark-202607 service-quotas get-service-quota --region us-east-2 --service-code ec2 --quota-code L-1216C47A --query 'Quota.Value' --output text
```

Expected: the dedicated benchmark principal selected by the private manifest and enough vCPU for the retained central host plus one `m7i-flex.large` sandbox plus headroom. Store only a redacted identity proof.

- [ ] **Step 2: Freeze unique readiness identities**

Use exactly:

```text
fullrun-readiness-claude-fable5-20260719-v1   port 4380
fullrun-readiness-claude-sonnet5-20260719-v1  port 4381
fullrun-readiness-codex-sol-20260719-v1       port 4382
fullrun-readiness-cloud-kimi-20260719-v1      port 4383
```

Expected: no matching run root, DB, socket, listener, trace, controller event, Harbor directory, EC2 tag, EBS volume, or ENI exists.

- [ ] **Step 3: Verify secrets by presence and permission only**

On the central host, verify the daemon environment contains a non-empty `CLAUDE_CODE_OAUTH_TOKEN`, the protected Codex OAuth store is readable by the daemon user, and the protected BitRouter Cloud credential source is readable. Print only `present`/`missing` and file mode; never print contents, lengths, prefixes, suffixes, hashes, or decoded claims.

Expected: all three credential classes report present; secret files are owner-only.

- [ ] **Step 4: Validate four fixed-route configs**

Each config disables policy learning and maps one tier to exactly one target:

```text
claude-code:claude-fable-5
claude-code:claude-sonnet-5
openai-codex:gpt-5.6-sol
bitrouter:moonshotai/kimi-k2.7-code
```

Run the frozen binary's `config validate` against all four configs. Expected: four zero exits, unique database/socket/port/output paths, and no secret literal in any config.

- [ ] **Step 5: Run direct sentinels through the frozen daemon**

Start each daemon with its protected credential already present, issue one non-streaming request with a unique `-sentinel` run identity, stop gracefully, and export its trace/usage. Expected for every route: HTTP success, intended provider/model in the trace, numeric four usage buckets, terminal settlement, and no secret in logs.

---

### Task 4: Run four real Harbor/Terminus 2 EC2 canaries

**Files:**
- Validate at runtime: Harbor one-case `TrialConfig` JSON for each readiness identity
- Operator-private output: Harbor logs/results, trajectories, traces, usage, outcomes, controller events, and AWS cleanup proofs outside the repository

**Interfaces:**
- Consumes: exact binary/config/provider matrix from Task 3.
- Produces: one complete non-scoring TrialResult and strict end-to-end evidence bundle per route.

- [ ] **Step 1: Render and validate four TrialConfigs**

For each identity, use:

```yaml
agent:
  name: terminus-2
  model_name: openai/gpt-5.6-terra
  extra_allowed_hosts: [CENTRAL_PRIVATE_HOST]
  env:
    OPENAI_API_KEY: bitrouter-local
  kwargs:
    api_base: http://CENTRAL_PRIVATE_HOST:ROUTE_PORT/v1
    parser_name: json
    session_id: EXACT_TRIAL_SESSION
    llm_kwargs:
      api_key: bitrouter-local
    llm_call_kwargs:
      extra_headers:
        x-bitrouter-workflow-session: EXACT_TRIAL_SESSION
```

Replace the all-caps values before writing the operator-private configs. Validate each executable object with Harbor's pinned `TrialConfig.model_validate`. The two `claude-code` subscription candidates stop at the compatibility gate; only Codex/Cloud configs are executable unless Claude is replaced by an `anthropic` API-key route.

- [x] **Step 2: Run the Claude Fable 5 compatibility gate**

The gate rejected `claude-code:claude-fable-5` before creating a preflight/run root or AWS resource because Terminus 2 cannot supply a genuine Claude Code marker. The route is excluded unless replaced by an `anthropic` API-key route.

- [x] **Step 3: Run the Claude Sonnet 5 compatibility gate**

The same pre-resource compatibility gate rejected `claude-code:claude-sonnet-5`; no trial identity or AWS resource was consumed.

- [ ] **Step 4: Launch the Codex Sol canary**

Repeat only with the Codex identity/port/config. Expected: the same strict runtime result and a trace whose executed target is `openai-codex:gpt-5.6-sol` over Responses upstream.

- [ ] **Step 5: Launch the BitRouter Cloud Kimi canary**

Repeat only with the Cloud identity/port/config. Expected: the same strict runtime result and a trace whose executed target is `bitrouter:moonshotai/kimi-k2.7-code`.

- [ ] **Step 6: Prove cleanup after every canary**

For each exact run ID, query instances, attached/available volumes, and ENIs through `--profile benchmark-202607`. Expected after each canary: `[]` for all three resource classes and monitor peak/tail `1/0`.

---

### Task 5: Strictly audit trace, cache, settlement, and attribution

**Files:**
- Inspect: `apps/bitrouter/src/workflow_state/archive.rs`
- Inspect: `apps/bitrouter/src/metering/reader.rs`
- Operator-private output: selected trace, decision, usage, outcome, and bundle files for each route

**Interfaces:**
- Consumes: four completed canaries from Task 4.
- Produces: four accepted or rejected route-readiness decisions, with exact per-request evidence.

- [ ] **Step 1: Select exact request-ID sets**

Filter every trace by the exact canary run ID, exclude `-sentinel`, require unique stable request IDs, and require the decision request-ID set to equal the trace set when the config emits policy decisions.

Expected: no duplicate, missing, or extra request ID.

- [ ] **Step 2: Export and validate four-bucket usage**

Run the frozen binary's `workflow-state metering-usage` over each canary DB and exact start/finish window, then select only the exact trace request IDs.

For every row assert numeric non-negative:

```text
uncached_input_tokens
cache_read_tokens
cache_write_tokens
output_tokens
```

Expected: cache categories unsupported by a provider are authoritative observed zero, not absent/null; settlement is terminal `computed` or authoritative `not_charged`.

- [ ] **Step 3: Export outcomes and assemble strict bundles**

Run `workflow-state harbor-outcomes` and `workflow-state bundle` for each canary. Expected: one outcome, exact cost join, exact reward join, exact session join, no unmatched count, no unknown settlement request ID.

- [ ] **Step 4: Compare provider-specific evidence**

Record, without credentials:

```text
route, provider, executed model, protocol, request count,
uncached input, cache read, cache write, output,
usage origin, charge/reconciliation status, actual/notional class,
TrialResult status, reward, session confidence, cleanup result
```

Expected: all four routes have real traces and provider-appropriate authoritative usage. Do not combine subscription-counterfactual and actual Cloud charges into one total.

- [ ] **Step 5: Issue per-route terminal decisions**

Accept a route only when every Task 4–5 gate passes. On any failure, preserve the v1 evidence, mark the route rejected, fix the root cause using a new source/config hash, and use a new `-v2` canary identity; never overwrite or relaunch v1.

---

### Task 6: Update the benchmark skill, push PR #717, and issue GO/NO-GO

**Files:**
- Modify when warranted: `skills/run-bitrouter-benchmark/references/configuration.md`
- Modify when warranted: `skills/run-bitrouter-benchmark/references/operations.md`
- Modify when warranted: `skills/run-bitrouter-benchmark/references/qna.md`
- Modify if navigation changes: `skills/run-bitrouter-benchmark/SKILL.md`
- Modify: PR #717 body through GitHub
- Operator-private output: final readiness report and checksum manifest outside the repository

**Interfaces:**
- Consumes: merged branch tests and four strict canary decisions.
- Produces: updated PR #717 head, reusable skill guidance for new pitfalls, and a final immutable full-run manifest or a NO-GO report.

- [ ] **Step 1: Convert reusable findings into documentation-only guidance**

Document only findings observed in this run. Stable configuration facts go in `configuration.md`, lifecycle steps in `operations.md`, and symptom/cause/safe-action entries in `qna.md`. Include the headless Claude requirement that `CLAUDE_CODE_OAUTH_TOKEN` must exist before daemon construction if confirmed by the live path. Exclude private paths, identities, addresses, and credential material.

- [ ] **Step 2: Validate the skill**

Run the official skill validator available in the local Codex skill-creator package and:

```bash
git diff --check
rg -n '(AKIA|BEGIN .*PRIVATE KEY|CLAUDE_CODE_OAUTH_TOKEN=.+|[0-9]{1,3}(\.[0-9]{1,3}){3})' skills/run-bitrouter-benchmark
```

Expected: validator passes, diff check is clean, and the scan finds no credential value/private key/IP literal.

- [ ] **Step 3: Run final source gates after any repair**

Run:

```bash
cargo fmt --all -- --check
cargo test --all-features
cargo clippy --all-features -- -D warnings
cargo run -p dist-helper -- check
```

Expected: all exit zero on the exact commit to push.

- [ ] **Step 4: Commit and push to the existing PR head**

Use conventional commits for any repair/skill update, then push the verified integration branch to:

```bash
git push origin HEAD:codex/c0-c1-policy-router
```

Expected: PR #717 head equals the locally verified commit; no force push; GitHub reports the PR mergeable or only waiting for checks/review.

- [ ] **Step 5: Update PR #717 evidence**

Append the exact main commit, merged source/binary hash, test commands, four route decisions, token/cache evidence summary, and cleanup results. Never put secret-source paths or token information in the public PR.

- [ ] **Step 6: Freeze the scored full-run tuple**

Create the operator-private full-run manifest containing the exact 89-task ordered manifest, one predeclared trial identity per task, fixed concurrency, retry zero, provider/model routes, price snapshot, source/binary/Harbor hashes, AWS selector, paths, ports, timeouts, settlement grace, spend limit, quality stop, and cleanup rules.

Expected final decision:

- `GO` only if all four real-agent canaries are accepted, PR #717 checks pass, the PR head equals the frozen serving commit, and no runtime mutation remains planned.
- `NO-GO` otherwise; no scored full-run identity is started.
