# ACP Controller P0–P1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver Phase 0 and Phase 1 of the ACP controller spec: pinned and correctly configured Claude/Codex adapters plus a manager-first, connection-level ACP v1 controller that transparently forwards multiple harness-native sessions without storing or aliasing session data.

**Architecture:** Use the official Rust SDK 2.0 conductor and proxy roles as the connection kernel. A BitRouter proxy intercepts only initialization so it can relay manager client capabilities, configure a harness-advertised LLM provider, verify the effective non-secret endpoint, mask that internal provider capability from the manager, and identify the wrapper honestly. Every session method, callback, notification, response, unknown extension field, and native session ID otherwise passes through the conductor unchanged. The existing single-session engine remains available for `acp prompt` and `chat`; `acp serve` moves to the controller.

**Tech Stack:** Rust 2024, Tokio, `agent-client-protocol` 2.0, its release-pinned `agent-client-protocol-schema` 1.5, `agent-client-protocol-conductor` 2.0, serde/JSON, Cargo nextest/clippy/rustdoc.

**Spec:** [`docs/ACP_CONTROLLER_SPEC.md`](../../ACP_CONTROLLER_SPEC.md), especially §§5–9, 16–18, and Delivery Phases 0–1.

## Global Constraints

- Work only in the isolated `codex/acp-runtime-feasibility-20260901` worktree based on the latest fetched `origin/main` baseline.
- Follow strict red-green-refactor: add one behavior test, observe the intended failure, add the minimum implementation, and rerun the focused test before refactoring.
- Do not add a BitRouter session database, transcript cache, harness-home reader, manager-facing session alias, or persistent session mutation.
- Keep stable ACP v1 wire semantics. Do not enable ACP v2 wire negotiation.
- Treat adapter `providers/*` as controller-to-harness endpoint configuration; do not advertise it manager-side as BitRouter route selection.
- Keep `CLAUDECODE` and caller-configured inherited markers stripped from child environments. Do not redirect `CLAUDE_CONFIG_DIR` or `CODEX_HOME`.
- Never expose credentials through initialize metadata, provider verification responses, logs, errors, fixtures, or tests.
- Preserve current pure model API workflow-session extraction and routing semantics byte-for-behavior; P2 identity normalization and route leases are out of scope.
- Never add `#[allow]`, `.unwrap()`, `.expect()`, or `panic!` to production Rust. Keep public modules explicit; do not add public re-exports from public modules.
- Run localhost HTTP tests with `http_proxy`, `https_proxy`, and `all_proxy` (plus uppercase variants) unset; the developer machine proxy otherwise intercepts wiremock.
- Update `skills/bitrouter/` in the same change as CLI or harness wiring.
- Use conventional commit and PR titles under 60 characters.

---

### Task 1: Upgrade and lock the ACP runtime foundation

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/bitrouter-sdk/Cargo.toml`
- Modify: `apps/bitrouter/Cargo.toml`
- Modify as required by compile errors: `crates/bitrouter-sdk/src/acp/up.rs`
- Modify as required by compile errors: `crates/bitrouter-sdk/src/acp/down.rs`
- Modify as required by compile errors: ACP tests under `crates/bitrouter-sdk/src/acp/`

- [x] Add a compile-time characterization test that exercises the stable v1 initialize/new/prompt types through the current live runtime boundary; name the breaking change it catches: accidentally selecting or requiring v2 wire semantics.
- [x] Run the focused test on the old dependency graph and record the green compatibility baseline before the dependency-only migration.
- [x] Pin `agent-client-protocol = "=2.0.0"`, its exact compatible `agent-client-protocol-schema = "=1.5.0"` with `unstable_llm_providers` and `unstable_session_fork`, and `agent-client-protocol-conductor = "=2.0.0"` behind the SDK `acp` feature.
- [x] Run `cargo check -p bitrouter-sdk --features acp` and use compiler diagnostics to migrate SDK 1.2 API calls to 2.0 without changing behavior.
- [x] Run the focused ACP runtime tests, then `cargo nextest run -p bitrouter-sdk --all-features` (the crate's test target requires `config_file` in addition to `acp`).
- [x] Commit: `build(acp): upgrade controller runtime`

### Task 2: Define one harness endpoint plan and pin adapters

**Files:**

- Modify: `apps/bitrouter/src/harness.rs`
- Modify: `apps/bitrouter/src/acp_cli.rs`
- Modify: `apps/bitrouter/src/agents.rs`
- Modify: `apps/bitrouter/src/commands.rs`
- Modify: `apps/bitrouter/src/onboarding.rs`
- Add: `apps/bitrouter/tests/fixtures/acp_adapters/claude-agent-acp-0.70.0.json`
- Add: `apps/bitrouter/tests/fixtures/acp_adapters/codex-acp-1.7.0.json`
- Test: unit tests in `apps/bitrouter/src/harness.rs`

- [ ] Add failing catalog tests requiring `@agentclientprotocol/claude-agent-acp@0.70.0`, its maintained repository/marker, and `@agentclientprotocol/codex-acp@1.7.0`; verify the failure is the old package or `@latest`.
- [ ] Add failing tests for `HarnessEndpointPlan` requiring a protocol, exact adapter provider ID (`main` for Claude, `openai` for Codex), normalized `/v1` base URL, logical model, secret headers, and non-secret controller/harness headers.
- [ ] Add failing fallback-rendering tests: Claude emits `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, model, and newline-separated `ANTHROPIC_CUSTOM_HEADERS`; Codex emits `CODEX_CONFIG` JSON plus `MODEL_PROVIDER` and emits no `-c` arguments in ACP mode.
- [ ] Implement `HarnessEndpointPlan` and separate `Harness::acp_routing_overlay` from the existing interactive `routing_overlay`, keeping interactive Codex CLI `-c` behavior intact.
- [ ] Make ACP routing build and apply one plan, returning it with the routing result for controller-side provider configuration. Preserve `--direct` as no endpoint plan.
- [ ] Version adapter contract fixtures with package, provider ID, protocol, fallback keys, native session headers, and source revision; consume them in endpoint-plan tests rather than duplicating expectations.
- [ ] Run `cargo nextest run -p bitrouter harness` and the affected agents/onboarding tests.
- [ ] Commit: `feat(acp): pin harness endpoint plans`

### Task 3: Lock pure model API compatibility

**Files:**

- Add: `apps/bitrouter/tests/fixtures/workflow_state/pure_model_api_sessions.json`
- Modify: `apps/bitrouter/src/workflow_state/session.rs`
- Modify if required for a public test seam only: `apps/bitrouter/src/workflow_state/extractors.rs`

- [ ] Add a table-driven compatibility fixture covering explicit `x-bitrouter-workflow-session`, adapter body hints, first-user-message fallback, generic Responses continuation, Claude-like Messages traffic, Codex-like Responses traffic, and conflicting generic headers.
- [ ] Assert literal legacy `SessionSignal` key/source/confidence plus the route-relevant workflow identity projection for every case; do not derive expectations through extractor helpers.
- [ ] Run the new test before any workflow-state or route-policy production change and confirm it passes, proving the P0/P1 work has a fixed compatibility state independent of the adapter catalog migration.
- [ ] Rerun it after each controller integration task; any difference is a regression and must be fixed without changing the fixture.
- [ ] Commit: `test(router): lock pure API sessions`

### Task 4: Build the manager-first controller initialize gate

**Files:**

- Add: `crates/bitrouter-sdk/src/acp/controller.rs`
- Modify: `crates/bitrouter-sdk/src/acp/mod.rs`
- Modify: `crates/bitrouter-sdk/src/acp/up.rs`
- Modify: `crates/bitrouter-sdk/Cargo.toml`
- Test: controller unit/integration tests in `crates/bitrouter-sdk/src/acp/controller.rs`

- [ ] Add a failing in-memory test where the fake harness records initialize; assert the manager's client capabilities and `_meta` reach it exactly, and assert initialize is sent once only after the manager initializes.
- [ ] Extract a reusable child connector from the existing `up.rs` process transport so the conductor retains inherited-env stripping, stderr draining, process-group teardown, and no leaked nested-agent marker.
- [ ] Implement `Controller` over the official conductor with one upstream agent component and one BitRouter proxy. Do not create a `Session` or local session ID.
- [ ] Add a failing test where the harness advertises providers and refuses `session/new` until `providers/set`; assert manager initialize does not resolve until set succeeds.
- [ ] Implement provider setup using the plan's typed `SetProviderRequest`; when advertised, call `providers/set`, call `providers/list`, and compare provider ID, protocol, and base URL while never comparing or logging secret headers.
- [ ] Add failing error-path tests for provider set failure, provider list mismatch, and no advertised provider capability. The first two must fail initialize; the last must succeed using launch fallback.
- [ ] Add failing capability tests requiring upstream lifecycle capabilities to pass through while internal provider capability is removed manager-side.
- [ ] Set manager-facing `agentInfo` to the BitRouter ACP controller and put sanitized upstream identity, harness ID, and pinned adapter version under namespaced response `_meta`.
- [ ] Run controller tests and `cargo nextest run -p bitrouter-sdk --features acp`.
- [ ] Commit: `feat(acp): add controller initialize gate`

### Task 5: Prove transparent native multi-session lifecycle

**Files:**

- Modify: `crates/bitrouter-sdk/src/acp/controller.rs`
- Remove from the public serve path: `crates/bitrouter-sdk/src/acp/down.rs`
- Modify: `crates/bitrouter-sdk/src/acp/mod.rs`
- Test: controller conformance tests in `crates/bitrouter-sdk/src/acp/controller.rs`

- [ ] Add a failing two-session conformance test: one fake harness process returns `native-a` and `native-b`; manager new responses, prompts, updates, and cancellations must contain those exact IDs and no BitRouter UUID.
- [ ] Add failing lifecycle forwarding tests for list, load, resume, close, delete, fork, and set-config-option. Each request must reach the harness even when the controller did not previously observe its ID, and every response/error must remain harness-authored.
- [ ] Add a failing callback test for permission plus one filesystem or terminal request; manager capability advertisement must enable the harness callback and the response must complete through the conductor.
- [ ] Add a failing `_meta`/unknown-extension preservation test in both directions.
- [ ] Rely on conductor transparent forwarding for implementation; add only ephemeral observation hooks needed for shutdown or diagnostics, never a session catalog.
- [ ] Retire the single-session `down::serve` manager wire so no public BitRouter ACP endpoint can return `SessionState.record_id`. Keep the single-session engine only for local `prompt`/`chat` execution.
- [ ] Run focused controller conformance tests twice, including under concurrent prompt scheduling.
- [ ] Commit: `feat(acp): forward native multi-session lifecycle`

### Task 6: Wire `acp serve` and update operator guidance

**Files:**

- Modify: `apps/bitrouter/src/acp_cli.rs`
- Modify as needed: `apps/bitrouter/tests/acp.rs`
- Modify: `skills/bitrouter/SKILL.md`
- Modify: `skills/bitrouter/references/cli.md`
- Modify: `skills/bitrouter/references/providers.md`
- Modify: `docs/ACP_CONTROLLER_SPEC.md` only for implemented-status notes, not design changes

- [ ] Add a failing app integration test that launches `acp serve` against a deterministic stub harness, initializes manager-first, opens two native sessions, and exercises one resumed or loaded session.
- [ ] Replace `Session::launch_deferred` plus `down::serve_with` in `acp serve` with the connection-level controller, passing the routed endpoint plan and configured child environment.
- [ ] Keep `acp prompt` and `chat` on the current single-session engine; confirm their existing prompt, permission, timeout, and telemetry tests stay green.
- [ ] Remove manager-side `providers/*` advertisement from `acp serve`; document that standard providers configure the harness endpoint internally and BitRouter session route control remains P2.
- [ ] Update the skill runbook with exact pinned adapter commands, manager-first behavior, native IDs, multi-session lifecycle, Codex `CODEX_CONFIG` fallback, and the distinction between controller live state and harness session storage.
- [ ] Run app ACP tests, harness tests, skill reference checks, and the pure API compatibility matrix.
- [ ] Commit: `feat(cli): serve the ACP controller`

### Task 7: Review, verify, and publish

**Files:**

- Review all changed files
- Add fixes and regression tests beside the affected code

- [ ] Use `superpowers:requesting-code-review` to review the complete P0/P1 diff against the spec and this plan. Because no delegated subagents are authorized, perform the prescribed evidence-based review locally and record findings in the turn.
- [ ] For each finding, use `superpowers:receiving-code-review`: verify it against code/spec, add a reproducing test when behavioral, fix it, and rerun focused tests.
- [ ] Run `cargo fmt -- --check`.
- [ ] Run `env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo nextest run --all-features`.
- [ ] Run `env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo clippy --workspace --all-features --tests -- -D warnings`.
- [ ] Run `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`.
- [ ] Inspect `git diff --check`, `git status --short`, and the complete diff for credentials, generated artifacts, aliases, accidental session persistence, or unrelated user changes.
- [ ] Use `superpowers:verification-before-completion`, then `superpowers:finishing-a-development-branch`.
- [ ] Push `codex/acp-runtime-feasibility-20260901` and open one PR against `main` with a conventional title under 60 characters and a body that maps evidence to P0/P1, test gates, known P2 exclusions, and the localhost proxy test note.
