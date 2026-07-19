# Anthropic Ingress to Claude Subscription Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Terminus 2 and other non-Claude-Code agents call BitRouter through the standard Anthropic API while an explicit `claude-code:<model>` route is transformed into a valid Claude Code subscription OAuth request, then prove Fable 5 and Sonnet 5 in real EC2 end-to-end canaries.

**Architecture:** Keep subscription selection explicit and keep subscription providers outside the canonical auto-cascade. Remove only the downstream-agent identity gate from the `claude-code` auth applier; the applier owns the outbound OAuth/Claude-Code header transformation while the existing ingress detector continues providing automatic routing for genuine Claude Code clients. Deploy one content-addressed Linux binary to the central EC2 daemon and validate it with standard Anthropic sentinels plus Harbor/Terminus 2 ephemeral EC2 trials.

**Tech Stack:** Rust/Cargo, reqwest, BitRouter Anthropic Messages ingress and provider auth appliers, Markdown docs and Agent Skill, Python unittest/readiness driver, Harbor, Terminus 2, AWS EC2/STS/Service Quotas CLI, SQLite workflow-state evidence.

## Global Constraints

- The downstream contract is the standard Anthropic Messages API; Terminus 2 must not inject a Claude Code beta or identity prompt.
- Only an explicit `claude-code:<model>` target authorizes subscription use. Bare canonical Claude models remain excluded from subscription auto-cascade.
- `CLAUDE_CODE_OAUTH_TOKEN` is captured before daemon construction and is never printed, committed, archived, hashed, decoded, or placed in a process argument.
- The token is delivered to the central host through non-echoing standard input and stored only in an owner-only mode-`0600` environment file outside the repository and benchmark artifacts.
- AWS operations use the explicit IAM access-key profile `benchmark-202607` in `us-east-2`; ambient/default AWS identity is forbidden.
- Live validation uses Terminal-Bench 2.1 + Harbor + Terminus 2 + a central EC2 BitRouter daemon + ephemeral EC2 sandboxes.
- Controller capacity is 4. Each one-route canary uses one sandbox; the final capacity canary must observe four simultaneous sandboxes.
- Every code, binary, config, or input change receives a new immutable canary run ID. Never overwrite or relaunch rejected evidence.
- English and Chinese documentation remain structurally identical, and `skills/bitrouter/` plus `skills/run-bitrouter-benchmark/` remain aligned with shipped behavior.
- Production Rust contains no `unwrap`, `expect`, `panic!`, `#[allow]`, public re-export workaround, or unused abstraction.

---

### Task 1: Accept a standard Anthropic request on an explicit Claude subscription route

**Files:**
- Modify: `crates/bitrouter-providers/src/claude_code.rs`
- Test: `crates/bitrouter-providers/src/claude_code.rs`

**Interfaces:**
- Consumes: `ClaudeCodeAuthApplier::apply(reqwest::Request, &RoutingTarget)` and the existing `merged_beta_value` helper.
- Produces: an explicit `claude-code` target that always synthesizes the required upstream headers after credential resolution, independent of inbound Claude Code identity.

- [ ] **Step 1: Replace the old rejection test with a failing translation test**

Replace `apply_rejects_request_without_agent_profile_beta` with:

```rust
#[tokio::test]
async fn explicit_route_adds_agent_profile_to_standard_anthropic_request() {
    let path = tmp_store_path();
    let applier = ClaudeCodeAuthApplier::new(&path)
        .unwrap()
        .with_env_oauth_token(Some(OAuthToken {
            access_token: "sk-ant-oat-env".into(),
            expires_at: 0,
            refresh_token: None,
        }));
    let mut req = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .build()
        .unwrap();
    req.headers_mut()
        .insert("x-api-key", HeaderValue::from_static("downstream-key"));

    let authed = applier.apply(req, &cc_target(None)).await.unwrap();
    let headers = authed.headers();
    let beta = headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
        .unwrap();
    assert!(beta.contains("claude-code-20250219"));
    assert!(beta.contains("oauth-2025-04-20"));
    assert_eq!(
        headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer sk-ant-oat-env")
    );
    assert!(headers.get("x-api-key").is_none());
    assert_eq!(
        headers
            .get(reqwest::header::USER_AGENT)
            .and_then(|value| value.to_str().ok()),
        Some(headers::CLAUDE_CODE_USER_AGENT)
    );
    assert_eq!(
        headers.get("x-app").and_then(|value| value.to_str().ok()),
        Some(headers::CLAUDE_CODE_X_APP)
    );
}
```

- [ ] **Step 2: Run the new test and prove the current gate rejects it**

Run:

```bash
cargo test -p bitrouter-providers explicit_route_adds_agent_profile_to_standard_anthropic_request -- --exact --nocapture
```

Expected: FAIL with the current `missing the Claude Code agent-profile beta` error.

- [ ] **Step 3: Remove only the inbound identity gate**

Delete the `request_has_claude_code_beta` early-return block and remove the now-unused private helper. Keep `beta_value_has_claude_code` because app-layer ingress detection/tests may still consume it if referenced. Update `apply` documentation to state:

```rust
// Reaching this applier means routing already resolved an explicit
// `claude-code:<model>` target. That explicit target is the subscription-use
// boundary. Downstream clients speak normal Anthropic Messages; this applier
// owns the OAuth/Claude-Code upstream transformation below.
```

Do not change credential resolution, beta merging, automatic ingress routing,
or routing-table subscription exclusion. Real-upstream validation later
amended the original body-passthrough assumption: the upstream requires a
recognized Claude Agent SDK identity. Add that identity centrally, preserve all
client system instructions, and keep genuine/legacy Claude Code idempotent.

- [ ] **Step 4: Run focused Claude provider tests**

Run:

```bash
cargo test -p bitrouter-providers claude_code
cargo test -p bitrouter claude_code
cargo test -p bitrouter-sdk subscription_provider_reachable_by_explicit_route_and_only
cargo test -p bitrouter-sdk strategy_3_excludes_subscription_providers_from_cascade
```

Expected: PASS; the new no-beta test proves translation, credential absence still proves `401`, feature-beta merging passes, and canonical auto-cascade remains excluded.

- [ ] **Step 5: Commit the core behavior**

Run:

```bash
git add crates/bitrouter-providers/src/claude_code.rs
git commit -m "fix(claude-code): bridge Anthropic clients"
```

Expected: one conventional commit containing only the tested provider behavior and its unit tests.

---

### Task 2: Align product docs and reusable benchmark knowledge

**Files:**
- Modify: `apps/bitrouter/src/claude_code.rs`
- Modify: `crates/bitrouter-providers/src/anthropic/headers.rs`
- Modify: `docs/integrations/claude-subscription.md`
- Modify: `docs/integrations/claude-subscription.zh.md`
- Modify: `skills/bitrouter/references/providers.md`
- Modify: `skills/run-bitrouter-benchmark/references/configuration.md`
- Modify: `skills/run-bitrouter-benchmark/references/operations.md`
- Modify: `skills/run-bitrouter-benchmark/references/qna.md`
- Test: documentation and skill validation commands discovered from their package metadata

**Interfaces:**
- Consumes: explicit-route behavior from Task 1.
- Produces: one consistent public contract and an operator Q&A that no longer rejects Terminus 2 subscription routing.

- [ ] **Step 1: Correct stale source comments without changing ingress behavior**

Update app/provider comments to distinguish the two paths:

```text
automatic route: inbound claude-code beta + bare Claude model -> claude-code:<model>
explicit route: claude-code:<model> -> auth applier synthesizes required upstream OAuth shape
```

Remove statements that the applier refuses to fabricate the required upstream profile. Do not alter `ClaudeCodeRouter::apply_with_headers` or `headers_indicate_claude_code`.

- [ ] **Step 2: Document long-lived token and non-Claude-Code callers in English**

Add an environment-token subsection to `docs/integrations/claude-subscription.md` with this executable pattern and no literal credential:

```bash
CLAUDE_CODE_OAUTH_TOKEN="$CLAUDE_CODE_OAUTH_TOKEN" bitrouter start
```

State that standard Anthropic clients may use an explicit `claude-code:<model>` route, BitRouter supplies the upstream Claude Code OAuth headers, and bare Claude requests never silently spend the subscription.

- [ ] **Step 3: Mirror the exact structure in Simplified Chinese**

Add the same heading, callout, code block, route examples, and links to `docs/integrations/claude-subscription.zh.md`. Translate prose only and leave `sourceHash` untouched.

- [ ] **Step 4: Replace the obsolete benchmark incompatibility Q&A**

Q&A must answer:

```text
Q: Can Terminus 2 or another non-Claude-Code harness use claude-code subscription models?
A: Yes, when the BitRouter policy or request explicitly resolves to
   claude-code:<model>. The harness speaks normal Anthropic/OpenAI-compatible
   ingress; BitRouter constructs the OAuth-compatible Claude Code upstream
   request. Never add the OAuth token or Claude Code identity headers to Harbor
   or sandbox configuration. Bare canonical Claude routes remain excluded from
   subscription auto-cascade.
```

Update configuration/operations with presence-only credential preflight, pre-start capture, owner-only central environment storage, direct no-beta sentinel, immutable run ID, and EC2 cleanup rules.

- [ ] **Step 5: Run documentation and skill checks**

Run:

```bash
cargo run -p dist-helper -- check
rg -n "only genuine Claude Code traffic may spend|provider only accepts Claude Code requests" docs/integrations skills apps crates
git diff --check
```

Expected: dist-helper exits zero; stale incompatibility assertions produce no output; diff check exits zero.

- [ ] **Step 6: Commit docs and skill alignment**

Run:

```bash
git add apps/bitrouter/src/claude_code.rs crates/bitrouter-providers/src/anthropic/headers.rs docs/integrations/claude-subscription.md docs/integrations/claude-subscription.zh.md skills/bitrouter skills/run-bitrouter-benchmark
git commit -m "docs: explain Claude subscription bridge"
```

Expected: one conventional commit with no credential literal and structurally matched English/Chinese docs.

---

### Task 3: Convert the readiness driver from rejection to execution

**Files:**
- Modify: `/private/tmp/bitrouter-fullrun-readiness/run_route_readiness_canary.py`
- Modify: `/private/tmp/bitrouter-fullrun-readiness/run_route_readiness_runtime.py`
- Modify: `/private/tmp/bitrouter-fullrun-readiness/test_run_route_readiness_canary.py`
- Deploy: `/home/ubuntu/bitrouter-bench/fullrun-readiness/drivers/`

**Interfaces:**
- Consumes: Fable 5 and Sonnet 5 route specs, fixed controller capacity 4, protected central credential environment, and strict `assess_acceptance` evidence schema.
- Produces: immutable executable Claude canaries with no inbound Claude Code beta and no secret-bearing artifacts.

- [ ] **Step 1: Invert the topology test first**

Replace the rejection test with:

```python
def test_terminus2_accepts_explicit_claude_code_routes(self):
    for route in ("claude-fable5", "claude-sonnet5"):
        self.assertIsNone(driver.topology_error(driver.ROUTES[route]))
```

Add assertions that both sentinel command arrays and Terminus 2 extra headers omit `anthropic-beta`, `CLAUDE_CODE_OAUTH_TOKEN`, and every `sk-` value.

- [ ] **Step 2: Run the driver unit test and prove it fails**

Run:

```bash
cd /private/tmp/bitrouter-fullrun-readiness
python3 -m unittest -v test_run_route_readiness_canary.py
```

Expected: FAIL because `topology_error` still rejects both Claude routes.

- [ ] **Step 3: Remove the obsolete topology rejection and advance identities**

Make `topology_error` return `None` for the existing route matrix. Advance both Claude route run IDs and preflight versions so the previously rejected identities are preserved rather than reused. Keep ports unique, `max_parallel_sandboxes` equal to 4, one case, zero retry, and standard downstream headers.

- [ ] **Step 4: Run driver tests and secret scan**

Run:

```bash
cd /private/tmp/bitrouter-fullrun-readiness
python3 -m unittest -v test_run_route_readiness_canary.py
rg -n "sk-ant-|CLAUDE_CODE_OAUTH_TOKEN=.*[^)]" .
```

Expected: all tests pass; the scan finds only variable names, presence checks, or synthetic test values and no live token.

- [ ] **Step 5: Deploy the driver atomically**

Copy the three verified files to a new content-addressed directory under `/home/ubuntu/bitrouter-bench/fullrun-readiness/drivers/`, then atomically update the driver symlink. Run remote unit tests before any run root is created.

Expected: remote tests pass and old driver directories remain immutable for audit.

---

### Task 4: Run repository-wide verification and build the Linux artifact

**Files:**
- Verify: all repository source, docs, registry, and tests
- Output only: release Linux `bitrouter` binary

**Interfaces:**
- Consumes: clean commits from Tasks 1–2.
- Produces: one source commit, Cargo.lock hash, and Linux binary SHA-256 used by every live canary.

- [ ] **Step 1: Run required repository gates**

Run:

```bash
cargo nextest run --all-features
cargo clippy --all-features -- -D warnings
cargo fmt --all -- --check
cargo run -p dist-helper -- check
git diff --check
```

Expected: every command exits zero. If `cargo-nextest` is unavailable, run `cargo test --all-features` and record that substitution.

- [ ] **Step 2: Build the Linux release binary from the clean commit**

Use the repository's established Linux build path from the prior readiness run. Record:

```bash
git rev-parse HEAD
shasum -a 256 Cargo.lock
shasum -a 256 target/release/bitrouter
```

Expected: the artifact is an ELF executable and all three identifiers are stable on repeated reads.

- [ ] **Step 3: Deploy under a content-addressed filename**

Upload to:

```text
/home/ubuntu/bitrouter-bench/fullrun-readiness/bin/bitrouter-<sha-prefix>
```

Verify remote SHA-256 and ELF magic before updating the readiness runtime's expected source/binary constants.

---

### Task 5: Inject the protected OAuth token and run direct Anthropic sentinels

**Files:**
- Create outside repository: central owner-only Claude environment file
- Create outside repository: two immutable direct-sentinel evidence bundles

**Interfaces:**
- Consumes: the user-supplied long-lived token through non-echoing stdin and the content-addressed binary from Task 4.
- Produces: no-beta standard Anthropic requests that execute Fable 5 and Sonnet 5 through the Claude subscription.

- [ ] **Step 1: Prove AWS identity, quota, and zero stale resources**

Run all AWS commands with `--profile benchmark-202607 --region us-east-2`. Expected: the dedicated IAM user, quota sufficient for central host plus four sandboxes, and no live instance/EBS/ENI carrying either new run ID.

- [ ] **Step 2: Deliver the token without argv or echo exposure**

Open an interactive SSH process whose remote shell disables echo, reads exactly one line from stdin, writes `CLAUDE_CODE_OAUTH_TOKEN=<value>` atomically to an owner-only file with `umask 077`, closes stdin, and reports only file mode plus `present`. Feed the secret only through the existing PTY `write_stdin` channel.

Expected: mode `0600`, non-empty variable on a source-and-test presence check, no value in local/remote shell history, process list, logs, or tool output.

- [ ] **Step 3: Start isolated daemons with the environment captured at construction**

For each model, source the protected file inside the remote service shell before executing the content-addressed BitRouter binary. Use unique port, socket, database, trace path, and run root. Validate the config before daemon start.

- [ ] **Step 4: Send standard Anthropic Messages requests**

Each sentinel uses `/v1/messages`, a provider-qualified `claude-code:<model>` value, `anthropic-version`, and a short user message. It deliberately omits `anthropic-beta`, Claude Code system identity, and upstream OAuth credentials.

Expected: HTTP success, intended model response, and trace evidence showing provider `claude-code` and the exact executed model for both Fable 5 and Sonnet 5.

- [ ] **Step 5: Inspect redacted daemon evidence**

Require Bearer application success without ever logging its value, no forwarded `x-api-key`, numeric four-bucket usage, and terminal subscription/notional settlement. Scan configs/logs/traces/evidence for the live token using an in-memory comparison that reports only `clean` or `contaminated`.

---

### Task 6: Run real Terminus 2 + Harbor + EC2 canaries

**Files:**
- Execute outside repository: immutable Fable 5 and Sonnet 5 run roots
- Update after acceptance: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/benchmark-runtime-repair-20260716/evidence/FULL-RUN-READINESS-20260720.md`
- Update after acceptance: `/Users/archer/Documents/aimonetwork/product-engineering/cost-optimization/runs/terminus2-terra-cc4af6d-short13-20260714T194346Z/RUN-STATUS.md`

**Interfaces:**
- Consumes: exact binary, protected daemon credential, remote driver, and accepted direct sentinels.
- Produces: strict real-agent EC2 acceptance evidence for both Claude models and corrected cost-optimization status.

- [ ] **Step 1: Execute the Fable 5 one-route canary**

Run the remote readiness runtime for the new Fable identity. Expected: exactly one `started` and one `terminal_valid`, one complete TrialResult with reward, exact trace/decision/session request-ID membership, four numeric usage buckets, terminal subscription settlement, exact cost/reward joins, sandbox peak/tail `1/0`, and zero EC2/EBS/ENI residue.

- [ ] **Step 2: Execute the Sonnet 5 one-route canary**

Repeat with the Sonnet identity and no shared run root, DB, socket, port, trace, or artifact. Apply the same strict acceptance gate.

- [ ] **Step 3: Diagnose every rejection before retrying**

Classify the first failing boundary from preserved evidence: ingress, route resolution, outbound auth shape, upstream model/auth, Harbor agent, TrialResult, usage/settlement, join, or cleanup. Add the smallest failing test at that boundary, repair it, rebuild/redeploy under a new SHA, and use new run IDs. Never retry the same immutable case identity.

- [ ] **Step 4: Exercise controller capacity 4**

After both one-route canaries are stable, create four non-scoring predeclared cases across the accepted Claude route(s), set `max_parallel_sandboxes` to 4, and launch once. Expected: observed sandbox peak 4, four terminal-valid TrialResults, no retry, strict per-case joins, tail 0, and zero residue.

- [ ] **Step 5: Update the cost-optimization status documents**

Replace the obsolete incompatibility conclusion with source commit, binary hash, immutable run IDs, provider/model matrix, exact acceptance results, concurrency peak/tail, and cleanup proof. Do not include credentials, private key paths, account identifiers, or secret-bearing command examples.

---

### Task 7: Final audit, publish the branch, and report readiness

**Files:**
- Inspect: entire Git diff since `1975d80e5`
- Update if needed: PR #717 description/evidence summary

**Interfaces:**
- Consumes: all passing repository gates and accepted EC2 evidence.
- Produces: a pushed PR branch and an evidence-backed final Claude readiness statement.

- [ ] **Step 1: Re-run final verification on the exact pushed candidate**

Run:

```bash
cargo nextest run --all-features
cargo clippy --all-features -- -D warnings
cargo fmt --all -- --check
cargo run -p dist-helper -- check
git status --short --branch
```

Expected: all pass and the working tree is clean.

- [ ] **Step 2: Audit secret absence and documentation consistency**

Compare the Git diff and retained evidence against the live token in memory, returning only `clean`/`contaminated`. Confirm English/Chinese structure, benchmark Q&A, explicit-route boundary, and canonical subscription exclusion all agree.

- [ ] **Step 3: Push and update PR #717**

Push the current branch to the PR head only after all tests and EC2 canaries pass. Update the PR summary with source/binary identities, Fable/Sonnet results, four-bucket settlement, strict joins, concurrency-4 evidence, and zero cleanup residue.

- [ ] **Step 4: Issue the terminal result**

Report success only if both models and the capacity-4 canary are accepted in real EC2. Otherwise report the exact first failing boundary and keep iterating under new immutable identities; do not downgrade direct sentinels or local tests into an end-to-end success claim.
