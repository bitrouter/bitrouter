# Benchmark Failure Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make BitRouter distinguish first-party routing failures from third-party provider failures, stop repeatedly selecting a known-bad route shape, and reject an unsafe benchmark before scored identities start—without using task content, task IDs, rewards, or benchmark labels as routing inputs.

**Architecture:** First eliminate the confirmed authentication failure by making OAuth refresh rotation atomic across Cloud replicas, serializing refresh across OSS processes that share one credential file, and requiring preflight to use the exact credential source used by scored daemons. Then add end-to-end request correlation and credential-scoped provider-attempt receipts, scope Cloud health by a content-free request-shape key, and activate the OSS reliability safety clamp. A reusable provider-check matrix exercises both credential usability and protocol capabilities before a benchmark begins.

**Tech Stack:** Rust, Tokio, Axum, Reqwest, SeaORM, SQLite/PostgreSQL, Clap, BitRouter language-model pipeline hooks, Harbor Terminus-2.

## Global Constraints

- Routing and health keys may contain only model, provider, credential class, endpoint origin, protocol, streaming flag, tool-presence flags, history class, and coarse input-size bucket. They must not contain prompt text, task/run/trial/case IDs, session IDs, rewards, verifier output, or hashes derived from any of those values.
- Frozen and adaptive policy modes use the same reliability safety plane. Policy learning remains disabled at request time; reliability is operational safety, not learning.
- A stored OAuth credential that exists but cannot refresh must never silently fall through to an inline API key. The operator must explicitly select the inline source or remove/re-authenticate the stored credential, preserving billing and Responses continuation authority.
- OAuth refresh rotation must be safe across Cloud replicas and independent OSS processes. A benign concurrent refresh loser must not revoke the successor family, and copied rotating credentials must not be treated as an isolated benchmark credential source.
- Public errors and receipts must never contain bearer tokens, refresh tokens, raw upstream bodies, prompt/tool arguments, or full request payloads.
- Preserve the existing 128-byte request-ID bound and credential-scoped receipt authorization.
- Base Cloud implementation on `bitrouter-cloud` `main` after PR #603, because that PR already supplies atomic actual-hop admission, exact half-open leases, auditable route snapshots, and billing fences.
- Follow `AGENTS.md`: no `#[allow(...)]`, no panic-based control flow, no unused abstractions, conventional commits under 60 characters, and update `skills/bitrouter/` whenever CLI or harness behavior changes.
- During a scored benchmark, never retry or relaunch an identity once started; preflight failure must happen before identities are consumed. Frozen timeouts and case lists never change in flight.
- Every implementation PR and deployment checkpoint must be linked from BitRouter OSS PR #766, which remains the coordination record.

---

## Evidence and Root-Cause Boundary

The replicated short run `pr766-replicated-short13-20260804T073418Z` at OSS commit `7a4aac07` produced:

- 39/39 identities started and finished exactly once; three trials ran in parallel and each reached four live case slots.
- 226 logical requests: 159 successes, 52 `upstream_bad_gateway`, and 15 `upstream_timeout`.
- All 52 gateway failures were on BitRouter Cloud routes: 34 `moonshotai/kimi-k3`, 18 `deepseek/deepseek-v4-pro`.
- OpenAI Codex had 159 successes and 15 intermittent timeouts; this is a degraded third-party route, not a total outage.
- 17 runtime-invalid cases: 16 Harbor `AgentTimeoutError`s and one setup `RuntimeError` caused by a remote LiteLLM cost-map TLS/read failure followed by missing `tmux`.
- The 52 Cloud-route failures were recorded locally from `2026-08-04T09:04:25Z` through `10:27:14Z`. Railway production logs contain 47 matching `POST /oauth/token` responses from `bitrouter-cloud-sdk/1.0.0-alpha.27`; every one returned HTTP 400 with the 70-byte canonical `invalid_grant` / `refresh token rejected` body. Forty-six local failures start within two seconds of one of those refresh responses and 47 within five seconds.
- The remaining five unmatched requests lasted about 30 seconds and also never reached inference ingress. Railway does not prove whether they timed out during metadata/refresh transport or another pre-inference auth step, so their exact sub-cause remains unassigned.
- The same Railway source made exactly three successful authorization-server metadata reads and zero `/v1/responses` or `/v1/chat/completions` calls during the scored failure window. Kimi and DeepSeek therefore received none of these 52 requests.
- At `07:50:04Z`, before the accepted revision-2 provider gate, four concurrent SDK refreshes raced: one returned 200 and the next three returned the same 400 `invalid_grant`. Current Cloud `rotateRefresh` mints a successor before atomically consuming the prior token; every loser of that consume race calls `revokeRefreshFamily`, revoking the winner's family. This behavior is still present on Cloud `origin/main` `44dd1a59`.
- Revision-2 sentinels later passed, but Railway shows successful provider-gate inference under a credential context different from the scored refresh path. The gate therefore proved provider availability, not scored credential usability. Real Terminus canaries exercised the strong route and likewise did not validate the later cheap-route auth source.
- Final settlement independently failed closed with the same OAuth `invalid_grant`; 52 receipts stayed pending and no local database rows were mutated.
- The public Cloud catalog currently reports one online platform candidate for `moonshotai/kimi-k3` and eight for `deepseek/deepseek-v4-pro`.
- Cloud already wires provider fallback, circuit breaking, and per-hop persistence, but none of those mechanisms ran for these 52 requests because authentication failed before inference ingress.
- Cloud still does not return its pipeline request ID as a stable response header, and its settlement endpoint does not expose actual provider-attempt outcomes. That remains a hardening gap for future genuine provider failures, not the blocker that prevented attribution in this incident.
- OSS loads stored account credentials before inline `BITROUTER_API_KEY`; refresh failure is wrapped as `BitrouterError::Upstream { status: 401, ... }`, which renders as 502 `upstream_bad_gateway`. Railway now confirms that this first-party auth/error-taxonomy path explains at least the 47 directly correlated failures and the absence of all 52 requests from inference ingress.
- OSS contains a persisted `ProviderReliabilityLedger`, but production assembly never observes hop outcomes into it and never consults it during model selection. All three scored databases contained zero `adequacy_reliability_events` despite 67 provider failures.

The repair now starts with the auth incident: fix atomic refresh rotation, credential-source isolation, cross-process refresh coordination, typed errors, and same-context preflight. Request correlation, capability-safe fallback, and the OSS reliability clamp remain required hardening, but Kimi/DeepSeek vendor remediation is not justified by this run because their inference endpoints were never called.

---

### Task 0: Repair OAuth Rotation and Credential-Context Admission

**Files:**

- Modify (Cloud): `console/src/lib/credentials/oauth-tokens.ts`
- Modify (Cloud): `console/src/lib/credentials/oauth-tokens.test.ts`
- Modify (Cloud): `console/src/app/oauth/token/route.test.ts`
- Modify (OSS): `crates/bitrouter-cloud-sdk/src/provider/applier.rs`
- Modify (OSS): `crates/bitrouter-cloud-sdk/src/auth/credentials.rs`
- Modify (OSS): `crates/bitrouter-cloud-sdk/tests/oauth_device_flow.rs`
- Modify (OSS): `skills/bitrouter/references/harness-terminus-2.md`
- Modify (OSS): `skills/bitrouter/references/diagnose.md`

**Interfaces:**

- Produces one atomic Cloud refresh result: one concurrent caller rotates successfully; losers receive typed `invalid_grant`; the winning successor family remains live.
- Produces cross-process refresh serialization for independent OSS resolver instances sharing one canonical credential path.
- Produces a content-free credential-context record containing source kind, authorization-server origin, namespace/subject binding presence, and a one-way authority identifier; it never contains either token.
- Requires benchmark preflight and scored daemons to present the same credential-context record. A mismatch rejects the run before any scored identity starts.

- [ ] **Step 1: Add a four-way Cloud refresh race regression test**

Mint one token family, issue four concurrent refresh requests with the same refresh token, and assert exactly one success plus three `invalid_grant` responses. Fetch the family rows after the race and assert exactly one unconsumed, unrevoked successor exists and the family is not revoked. Repeat with a delayed stale replay and assert it cannot invalidate the live successor.

- [ ] **Step 2: Make rotation one database transaction**

Move lookup, secret verification, consume claim, successor insert, and `rotated_to` linkage into one database transaction. Lock or atomically claim the presented row before minting the successor. A caller that observes an already-consumed row returns `RefreshInvalidError` without calling `revokeRefreshFamily`; explicit compromise/admin revocation remains a separate operation. Roll back the consume if successor minting fails.

- [ ] **Step 3: Run the Cloud auth suite**

Run:

```bash
bun test console/src/lib/credentials/oauth-tokens.test.ts \
  console/src/app/oauth/token/route.test.ts
```

Expected: the four-way race is deterministic, leaves one usable successor, and never logs or returns a token in diagnostics.

- [ ] **Step 4: Add an OSS cross-process refresh regression**

Construct two independent `BitrouterCloudAuthApplier` instances pointing at the same credential file and release them concurrently against a rotating Wiremock endpoint. Assert one network refresh, two usable bearer results, one atomic credential-file update, and no stale-token retry. The test must use separate resolver instances so the existing in-memory `tokio::Mutex` cannot make it pass accidentally.

- [ ] **Step 5: Serialize refresh by canonical credential path**

Acquire an OS-visible credential refresh lock, re-read the file after acquiring it, refresh only if the re-read token is still near expiry, and atomically persist the rotation before releasing the lock. Keep the current in-process mutex only as a fast path. If distinct files contain the same rotating token, rely on the repaired Cloud transaction for safety and report the stale copy as reauthentication required.

- [ ] **Step 6: Freeze and verify the exact credential context**

Run `cloud whoami --check` and the provider matrix under the exact environment used to launch each daemon. Record only the source kind and authority identifier. For automated benchmarks, explicitly select `inline` for a frozen static API key or `stored` for one live OAuth authority; never let an ambient default credential silently shadow the declared source. Reject copied OAuth stores and any preflight/scored source mismatch before allocating identities.

- [ ] **Step 7: Commit and deploy the incident fix first**

```bash
git add console/src/lib/credentials/oauth-tokens.ts \
  console/src/lib/credentials/oauth-tokens.test.ts \
  console/src/app/oauth/token/route.test.ts
git commit -m "fix(auth): make refresh rotation atomic"

git add crates/bitrouter-cloud-sdk skills/bitrouter/references
git commit -m "fix(auth): serialize cloud token refresh"
```

Deploy Cloud first, verify the four-way production-safe canary against fresh disposable credentials, then deploy the OSS resolver and rerun the same-context credential gate. Do not rerun scored cases until both sides pass.

---

### Task 1: End-to-End Request Correlation and Attempt Receipts

**Files:**

- Modify (OSS): `crates/bitrouter-sdk/src/server.rs`
- Modify (OSS): `crates/bitrouter-sdk/src/language_model/executor.rs`
- Modify (OSS): `crates/bitrouter-sdk/src/language_model/tests.rs`
- Modify (Cloud, after PR #603): `src/db/request_receipt.rs`
- Modify (Cloud, after PR #603): `src/db/request_settlement.rs`
- Create (Cloud, after PR #603): `src/db/migration/m20260804_000055_add_provider_attempt_diagnostics.rs`
- Modify (Cloud, after PR #603): `src/db/migration/mod.rs`
- Modify (Cloud, after PR #603): `src/service/provider_router/observation.rs`
- Modify (Cloud, after PR #603): `src/v1/http/management/settlement_receipts.rs`
- Modify (Cloud, after PR #603): `src/v1/http/management/integration_tests.rs`
- Modify (Cloud, after PR #603): `src/openapi/schemas.rs`
- Modify (Cloud, after PR #603): `src/openapi/operations.rs`
- Modify (Cloud, after PR #603): `specs/openapi.golden.yaml`

**Interfaces:**

- Produces `x-bitrouter-request-id` on every LLM HTTP response, including non-streaming errors and pre-stream failures.
- Produces first-party request propagation in `HttpExecutor`: when `target.provider_name == "bitrouter"`, set `x-bitrouter-request-id` from `PipelineContext::request_id()`; never forward this header to unrelated providers.
- Produces `ProviderAttemptReceipt { attempt_index, provider_id, canonical_model_id, provider_model_id, api_protocol, request_shape, status, status_code, error_code, duration_ms }` in the credential-scoped settlement response.
- Preserves the existing unique `(request_id, attempt_index)` provider-attempt identity and adds only content-free diagnostic columns.

- [ ] **Step 1: Write failing SDK tests for correlation propagation**

Add tests that create a request with `x-bitrouter-request-id: req-correlation-1`, assert the same header is present on successful and failed server responses, assert a BitRouter Cloud target receives the header upstream, and assert an `openai` target does not.

```rust
assert_eq!(response.headers()["x-bitrouter-request-id"], "req-correlation-1");
assert_eq!(captured_bitrouter_header.as_deref(), Some("req-correlation-1"));
assert_eq!(captured_openai_header, None);
```

- [ ] **Step 2: Run the focused SDK tests and verify they fail**

Run:

```bash
cargo nextest run -p bitrouter-sdk --all-features -E 'test(request_id)'
```

Expected: at least the response-header and first-party upstream propagation assertions fail.

- [ ] **Step 3: Implement response and first-party upstream correlation**

Add a small response helper in `server.rs` that inserts the validated request ID after every handler outcome. In `executor.rs`, insert the same header only for the `bitrouter` provider immediately before dispatch. Do not add a general observer-controlled arbitrary-header seam.

```rust
fn attach_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert(BITROUTER_REQUEST_ID_HEADER, value);
    }
    response
}
```

- [ ] **Step 4: Add Cloud provider-attempt diagnostic columns and typed rows**

Migration 55 adds non-null `api_protocol` and `request_shape` columns with migration-safe defaults of `unknown`. Extend `ProviderAttemptDoc` and the observer to write the actual outbound protocol and the shape key defined in Task 2. Do not store exact input bytes—store only the bucket token.

```rust
pub struct ProviderAttemptReceipt {
    pub attempt_index: i32,
    pub provider_id: String,
    pub canonical_model_id: String,
    pub provider_model_id: String,
    pub api_protocol: String,
    pub request_shape: String,
    pub status: String,
    pub status_code: Option<u16>,
    pub error_code: Option<String>,
    pub duration_ms: u64,
}
```

- [ ] **Step 5: Expose attempts through the credential-scoped settlement read**

Add `FabricDb::provider_attempts_for_credential(request_id, credential_id)` using the same request/credential ownership join as `request_settlement_for_credential`. Return `404` rather than an empty diagnostic object for a request owned by another credential. Add `attempts: Vec<ProviderAttemptReceipt>` to `SettlementResponse` and regenerate the OpenAPI golden file.

- [ ] **Step 6: Run correlation, authorization, migration, and OpenAPI tests**

Run in OSS:

```bash
cargo nextest run -p bitrouter-sdk --all-features -E 'test(request_id)'
```

Run in Cloud:

```bash
cargo nextest run --all-features \
  -E 'test(settlement_receipt) | test(provider_attempt) | test(request_id)'
cargo test --all-features openapi_golden
```

Expected: all pass; the cross-credential attempt test returns 404; no response includes raw upstream detail.

- [ ] **Step 7: Commit each repository independently**

```bash
git add crates/bitrouter-sdk/src/server.rs \
  crates/bitrouter-sdk/src/language_model/executor.rs \
  crates/bitrouter-sdk/src/language_model/tests.rs
git commit -m "feat: correlate cloud request receipts"

git add src/db src/service/provider_router/observation.rs \
  src/v1/http/management src/openapi specs/openapi.golden.yaml
git commit -m "feat: expose provider attempt receipts"
```

Use one conventional commit in each repository and link both commits from PR #766.

---

### Task 2: Shape-Scoped Cloud Health and Capability-Safe Fallback

**Files (Cloud, after PR #603):**

- Create: `src/service/provider_router/request_shape.rs`
- Modify: `src/service/provider_router/mod.rs`
- Modify: `src/service/provider_router/circuit_breaker.rs`
- Modify: `src/service/provider_router/admission_executor.rs`
- Modify: `src/service/provider_router/admission_hook.rs`
- Modify: `src/service/provider_router/observation.rs`
- Modify: `src/service/provider_router/metrics.rs`
- Modify: `src/v1/routing.rs`
- Modify: `src/main.rs`

**Interfaces:**

- Produces `RequestShapeKey`, derived only from structural request facts.
- Changes the circuit identity from `(provider, canonical_model)` to `(provider, canonical_model, outbound_protocol, request_shape)`; provider RPM remains provider-wide.
- Preserves anonymous fallback across the final admitted chain and the exact half-open lease semantics from Cloud PR #603.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct RequestShapeKey {
    pub inbound_protocol: String,
    pub streaming: bool,
    pub tools_declared: bool,
    pub tool_results_present: bool,
    pub multi_turn: bool,
    pub input_bucket: InputBucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum InputBucket { Le8K, Le32K, Le128K, Over128K }
```

- [ ] **Step 1: Write failing pure tests for stable request-shape classification**

Cover a single-turn plain request, multi-turn history, tool declaration, tool-result continuation, streaming, and each byte bucket. Prove that changing prompt text without changing structural facts yields the same key, while adding a tool result or crossing a bucket boundary changes it.

- [ ] **Step 2: Run the classifier tests and verify they fail**

Run:

```bash
cargo nextest run --all-features -E 'test(request_shape)'
```

Expected: compile failure because `request_shape` and `RequestShapeKey` do not exist.

- [ ] **Step 3: Implement the content-free classifier**

Compute the size bucket from canonical serialization length in memory, discard the exact length, and serialize only stable lower-snake-case tokens such as `responses_stream_tools_result_multi_le32k`. Never include message roles beyond the `multi_turn` boolean and `tool_results_present` boolean.

- [ ] **Step 4: Write failing circuit-isolation tests**

Add tests proving three `responses_*_tools_result_multi_le32k` failures open only that circuit, a `responses_nonstream_plain_single_le8k` success cannot close it, and one exact half-open probe is allowed for the failed shape.

```rust
assert_eq!(breaker.availability(&complex_key), CircuitAvailability::Blocked);
assert_eq!(breaker.availability(&plain_key), CircuitAvailability::Healthy);
```

- [ ] **Step 5: Thread the shape through route admission and observation**

Construct one `ProviderCircuitKey` per actual target using provider, canonical model, target protocol, and the request shape stored in `PipelineContext`. Use it for availability, probe claim/release, success, and failure. Persist the serialized shape and protocol in Task 1's attempt row.

- [ ] **Step 6: Preserve error-class boundaries**

Count 408, 429, 5xx, transport timeout, invalid response, and unavailable as transient health failures. Treat upstream 401/402/403/404 as persistent route/credential failures with immediate long cooldown. Do not count caller bad request, provider policy refusal, or an upstream payload-level 400 as shared health.

- [ ] **Step 7: Prove fallback with realistic structural fixtures**

Use mock upstreams and canonical fixtures for multi-turn history, tool declaration plus tool result, streaming, and 32 KiB context. The first provider returns 502 and the second succeeds; assert two attempt receipts, the serving provider, and no task content in either persisted attempt.

- [ ] **Step 8: Run Cloud provider-router verification**

Run:

```bash
cargo nextest run --all-features \
  -E 'test(provider_router) | test(routing) | test(fallback) | test(request_shape)'
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Expected: all pass.

- [ ] **Step 9: Commit the Cloud safety change**

```bash
git add src/service/provider_router src/v1/routing.rs src/main.rs
git commit -m "fix: scope provider health by request shape"
```

Open a Cloud PR based on PR #603's merged head and link it from PR #766.

---

### Task 3: Typed and Shared BitRouter Cloud Credential Resolution

**Files (OSS):**

- Modify: `crates/bitrouter-cloud-sdk/src/auth/flow.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/auth/credentials.rs`
- Create: `crates/bitrouter-cloud-sdk/src/auth/resolver.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/auth/mod.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/provider/applier.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/management/mod.rs`
- Modify: `crates/bitrouter-cloud-sdk/tests/oauth_device_flow.rs`
- Modify: `apps/bitrouter/src/cloud/cli.rs`
- Modify: `apps/bitrouter/src/cloud/mod.rs`
- Modify: `apps/bitrouter/src/main.rs`
- Modify: `docs/CLI.md`
- Modify: `skills/bitrouter/SKILL.md`
- Modify: `skills/bitrouter/references/cli.md`
- Modify: `skills/bitrouter/references/cloud-setup.md`
- Modify: `skills/bitrouter/references/diagnose.md`

**Interfaces:**

- Produces a typed refresh error rather than an `anyhow` string.
- Produces one `CloudCredentialResolver` used by inference, management, settlement reconciliation, and `cloud whoami --check`.
- Adds explicit environment selector `BITROUTER_CLOUD_CREDENTIAL_SOURCE=auto|stored|inline`; `auto` preserves stored-first behavior but fails closed if an existing stored credential is unusable.
- Produces the same content-free credential-context record defined in Task 0 so preflight, inference, and settlement can prove that they resolved the same authority without exposing a token.

```rust
pub enum RefreshError {
    TerminalOAuth { code: String, description: Option<String> },
    Transport { operation: &'static str, message: String },
    InvalidResponse { status: u16 },
}

pub enum CloudCredentialSource { Auto, Stored, Inline }

pub struct ResolvedCloudCredential {
    pub bearer: CloudBearer,
    pub source: ResolvedCloudCredentialSource,
    pub continuation_authority: Option<CredentialAuthority>,
}

pub struct CloudBearer(String); // Debug and Display always render <redacted>
```

- [ ] **Step 1: Write failing refresh-taxonomy tests**

Use Wiremock to return OAuth `invalid_grant`, token-endpoint 503, malformed JSON, and a successful rotated token. Assert `TerminalOAuth`, `Transport`/transient, `InvalidResponse`, and success respectively; assertions must not inspect or print token values.

- [ ] **Step 2: Implement typed refresh parsing**

Make `flow::refresh` return `Result<TokenSet, RefreshError>`. Preserve OAuth `error` and bounded `error_description` as typed fields, but make `Display` safe and free of endpoint bodies.

- [ ] **Step 3: Write failing credential-source tests**

Cover these exact cases:

1. no stored file + inline key under `auto` resolves inline;
2. valid stored OAuth + inline key under `auto` resolves stored;
3. stored OAuth `invalid_grant` + inline key under `auto` returns reauthentication required and does not fall through;
4. source `inline` ignores the stored file and resolves inline;
5. source `stored` with no file returns not signed in;
6. two independent resolvers sharing one credential path use Task 0's OS-visible lock, re-read after the lock, and perform exactly one refresh.

- [ ] **Step 4: Implement `CloudCredentialResolver` and reuse it**

Move metadata caching, Task 0's cross-process refresh serialization, persistence, source selection, credential-context reporting, and continuation-authority derivation behind the resolver. Replace `BitrouterCloudAuthApplier::resolve_stored_auth`, `ManagementClient`'s duplicate path, and `settlement_bearer_from_credentials` with the shared resolver.

- [ ] **Step 5: Map terminal OAuth failures to typed inference errors**

Map `invalid_grant`, expired refresh token, and missing stored refresh token to `BitrouterError::UpstreamAuth { status: 401, ... }`, yielding `upstream_auth_required`, not `upstream_bad_gateway`. Map metadata/token transport failures to `UpstreamUnavailable` or `UpstreamTimeout` according to the underlying error; never include the bearer or raw token response.

- [ ] **Step 6: Add `bitrouter cloud whoami --check`**

Change `CloudAction::Whoami` to `Whoami { check: bool }`. Without `--check`, retain the current offline behavior byte-for-byte. With `--check`, run the shared resolver and print only source, usable status, whether refresh was attempted, access-token expiry, the content-free authority identifier, and a remediation action such as `run bitrouter cloud login`; never print a token.

- [ ] **Step 7: Update CLI and skill documentation in lockstep**

Document `--check`, `BITROUTER_CLOUD_CREDENTIAL_SOURCE`, fail-closed stored-source behavior, and the exact remediation for `upstream_auth_required`. Keep `skills/bitrouter/SKILL.md` under 200 lines and place detail in references.

- [ ] **Step 8: Run focused and full auth checks**

Run:

```bash
cargo nextest run -p bitrouter-cloud-sdk --all-features
cargo nextest run -p bitrouter --all-features \
  -E 'test(cloud) | test(credential) | test(settlement)'
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Expected: all pass; a terminal stored OAuth failure is classified as `upstream_auth_required`, never uses the inline key under `auto`, and concurrent independent resolvers perform one safe refresh.

- [ ] **Step 9: Commit the OSS auth fix**

```bash
git add crates/bitrouter-cloud-sdk apps/bitrouter/src/cloud \
  apps/bitrouter/src/main.rs docs/CLI.md skills/bitrouter
git commit -m "fix(auth): type cloud credential failures"
```

Push to the OSS implementation branch and update PR #766 with the new behavior and migration/remediation notes.

---

### Task 4: Activate the Task-Neutral OSS Reliability Safety Plane

**Files (OSS):**

- Modify: `apps/bitrouter/src/adequacy/reliability.rs`
- Modify: `apps/bitrouter/src/adequacy/store.rs`
- Modify: `apps/bitrouter/src/adequacy/report.rs`
- Create: `apps/bitrouter/src/db/migration/m20260804_000014_rekey_reliability_events.rs`
- Modify: `apps/bitrouter/src/db/migration/mod.rs`
- Create: `apps/bitrouter/src/provider_reliability.rs`
- Modify: `apps/bitrouter/src/lib.rs`
- Modify: `apps/bitrouter/src/policy_table_router.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Modify: `apps/bitrouter/src/assemble.rs`
- Modify: `apps/bitrouter/tests/workflow_state_replay.rs`
- Create: `apps/bitrouter/tests/provider_reliability_runtime.rs`

**Interfaces:**

- Produces one event per actual hop rather than one event per request.
- Produces `ModelShapeKey { model, request_shape }`; it is the route circuit key and contains no policy fingerprint or task identity.
- Produces a `ReliabilityIntent` extension that records candidate model, certified baseline model, shape, and half-open status for the observer.
- Produces `ProviderReliabilityRuntime`, shared by policy selection and the hop observer.

```rust
pub struct ReliabilityEvent {
    pub event_id: String,
    pub request_id: String,
    pub attempt_index: u32,
    pub route_key: ModelShapeKey,
    pub endpoint_key: ReliabilityKey,
    pub observation: ReliabilityObservation,
    pub half_open_probe: bool,
    pub observed_at_unix: u64,
}

pub enum ReliabilityObservation { Success, TransientFailure, PersistentFailure }
```

`event_id` is `sha256("reliability-event/v1\0" || request_id || "\0" || decimal(attempt_index))`; it is an idempotency key only and never a routing input. `ModelShapeKey` is stored as `model || "\0" || request_shape`, so the persisted route key remains reversible without hashing request content.

- [ ] **Step 1: Write failing migration and store tests for multiple hops**

Insert two events with the same request ID and different attempt indexes; assert both persist and replay in order. Insert the same `event_id` twice with equal content and assert `Duplicate`; insert it with different content and assert a conflict.

- [ ] **Step 2: Add migration 14 and rekey the SeaORM entity**

Add `event_id` and `attempt_index`, make `event_id` unique, and remove uniqueness from `request_id`. Backfill existing rows with `event_id = 'legacy:' || sequence` and `attempt_index = 0`. Keep `sequence` as the ordered primary key.

- [ ] **Step 3: Extend the ledger for persistent failures and shape keys**

Transient failures use the configured threshold/cooldown. Persistent failures open immediately and use at least a one-hour cooldown. Success closes only the exact model-plus-shape circuit. Bump the reliability report schema and update digest fixtures.

- [ ] **Step 4: Write failing policy safety-clamp tests**

Create a policy whose candidate is `bitrouter:moonshotai/kimi-k3` and certified baseline is `openai-codex:gpt-5.6-sol`. Prove:

1. a closed complex-shape circuit selects Kimi;
2. after the threshold, the same shape selects the strong baseline;
3. a plain sentinel shape still selects Kimi and cannot close the complex circuit;
4. after cooldown exactly one complex request probes Kimi while concurrent requests select baseline;
5. frozen and adaptive modes produce identical choices;
6. changing task/session/request IDs cannot change the route key.

- [ ] **Step 5: Implement `ProviderReliabilityRuntime` and observer classification**

On each actual hop, derive `ReliabilityKey` from provider, provider model, shared/override credential class, endpoint origin only, outbound protocol, and Task 2's structural shape. Record successes; record 408/429/5xx, timeout, invalid response, and unavailable as transient; record upstream auth/payment/not-found as persistent; ignore caller 4xx and policy refusals.

- [ ] **Step 6: Apply the safety clamp before policy decision recording**

After the normal static/trajectory/tool guard produces its candidate, ask the runtime for a permit on `ModelShapeKey`. On `Open`, replace the selected tier/model with the certificate's baseline and record reason `reliability_fallback`. On `HalfOpenProbe`, retain the candidate and store `half_open_probe = true` in `ReliabilityIntent`. If an open policy route lacks a certified baseline, return `UpstreamUnavailable`; direct explicit-model requests remain unchanged.

- [ ] **Step 7: Replay history and wire the observer in production assembly**

In `assemble.rs`, load persisted events, build one shared runtime, pass it into `PolicyRuntime`, and register its `ObserveHook` independently of OTEL and independently of `PolicyRuntimeMode::apply_to_adequacy`. Persistence failure must be logged while the in-memory safety observation remains active; it must not convert a successful inference into an error.

- [ ] **Step 8: Run reliability and full OSS verification**

Run:

```bash
cargo nextest run -p bitrouter --all-features \
  -E 'test(provider_reliability) | test(reliability) | test(workflow_state_replay)'
cargo nextest run --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo run -p dist-helper -- check
```

Expected: all pass; integration tests produce non-zero reliability events after injected provider failures and select baseline only for the affected structural shape.

- [ ] **Step 9: Commit the OSS reliability change**

```bash
git add apps/bitrouter/src/adequacy apps/bitrouter/src/db/migration \
  apps/bitrouter/src/provider_reliability.rs apps/bitrouter/src/lib.rs \
  apps/bitrouter/src/policy_table_router.rs apps/bitrouter/src/policy_lock.rs \
  apps/bitrouter/src/assemble.rs apps/bitrouter/tests
git commit -m "feat: activate provider reliability safety"
```

Update PR #766 with event counts, exact safety semantics, and the fact that no task-derived value participates in the key.

---

### Task 5: Reusable Provider Capability Matrix and Benchmark Gate

**Files (OSS):**

- Create: `apps/bitrouter/src/provider_check.rs`
- Modify: `apps/bitrouter/src/lib.rs`
- Modify: `apps/bitrouter/src/main.rs`
- Create: `apps/bitrouter/tests/provider_check.rs`
- Modify: `docs/CLI.md`
- Modify: `skills/bitrouter/SKILL.md`
- Modify: `skills/bitrouter/references/cli.md`
- Modify: `skills/bitrouter/references/diagnose.md`
- Modify: `skills/bitrouter/references/harness-terminus-2.md`

**Interfaces:**

- Adds `bitrouter providers check --model <MODEL>... --via <BASE_URL> --concurrency 4 --context-bytes 8192,32768 --jsonl <PATH>`.
- Produces content-free rows: `case_id`, model, shape, concurrency slot, status, error code, request ID, response model, and latency.
- Sends no benchmark/task data; fixture text is a deterministic repeated ASCII token and tool names/arguments are fixed synthetic values.

- [ ] **Step 1: Write failing CLI parse and matrix-generation tests**

Assert repeated `--model`, default concurrency 4, context buckets 8 KiB and 32 KiB, and these mandatory shapes: plain single-turn, multi-turn history, tools declared, tool result continuation, and streaming. Assert generated rows and payloads contain none of `task`, `trial`, `case`, `reward`, or session identifiers.

- [ ] **Step 2: Implement the bounded synthetic matrix**

Build the five mandatory shapes for every model and size bucket. Use stable synthetic tool `echo_probe` with argument `{"value":"ok"}` and a matching tool result. Enforce bounded output tokens and an explicit per-request timeout; do not retry a failed matrix identity.

- [ ] **Step 3: Execute rows with a four-slot refill controller**

Keep exactly `min(concurrency, unfinished_rows)` live futures, launch the next row whenever one completes, and emit each terminal row once. A long row occupies only its own slot and cannot stop refill of other slots.

- [ ] **Step 4: Make the command fail closed**

Exit non-zero if any mandatory row has a non-2xx result, a protocol decode error, a missing request ID, a mismatched response shape, or a credential check failure. Print only content-free diagnostics and direct the operator to the Cloud attempt receipt by request ID.

- [ ] **Step 5: Add exact-daemon integration tests**

Run a local mock daemon where one model passes plain requests but fails tool-result history. Assert the matrix rejects it even though the plain sentinel succeeds. Run a second daemon where the first upstream fails and fallback succeeds; assert the row passes and retains the request ID for attempt inspection.

- [ ] **Step 6: Update CLI and Terminus skill documentation**

Require this sequence before any scored identities start, under the exact environment that will launch the daemon:

```bash
bitrouter cloud whoami --check
bitrouter providers check \
  --model bitrouter:moonshotai/kimi-k3 \
  --model bitrouter:deepseek/deepseek-v4-pro \
  --model openai-codex:gpt-5.6-sol \
  --via http://127.0.0.1:4356/v1 \
  --concurrency 4 \
  --context-bytes 8192,32768 \
  --jsonl artifacts/provider-check.jsonl
```

Document that the command incurs real provider traffic/cost and that a failed gate invalidates the run before identity launch. Persist the `whoami --check` credential-context record and require the daemon to report the same source kind and authority identifier. A provider response obtained with a different inline/stored credential context does not satisfy the gate.

- [ ] **Step 7: Run provider-check and docs verification**

Run:

```bash
cargo nextest run -p bitrouter --all-features -E 'test(provider_check)'
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Expected: all pass; the shallow-success/complex-failure fixture is rejected.

- [ ] **Step 8: Commit the gate**

```bash
git add apps/bitrouter/src/provider_check.rs apps/bitrouter/src/lib.rs \
  apps/bitrouter/src/main.rs apps/bitrouter/tests/provider_check.rs \
  docs/CLI.md skills/bitrouter
git commit -m "feat: add provider capability preflight"
```

Update PR #766 with the exact matrix artifact digest and gate result.

---

### Task 6: Deployment, Vendor Isolation, and Benchmark Re-Admission

**Files:**

- Modify (coordination): OSS PR #766 description and checkpoint comments
- Create (run artifact): immutable provider-check JSONL and checksum bundle
- Create (run artifact): credential-health JSON with tokens and paths redacted

**Interfaces:**

- Consumes atomic refresh behavior and credential-context admission from Task 0, Cloud provider-attempt receipts from Task 1, shape-scoped health from Task 2, typed credential health from Task 3, OSS safety decisions from Task 4, and the matrix gate from Task 5.
- Produces one of three explicit dispositions per failing tuple: first-party adapter/routing fix, third-party quarantine/escalation, or healthy/re-admitted.

- [ ] **Step 1: Land and deploy Cloud in dependency order**

First deploy Task 0's Cloud atomic-refresh fix and verify it with fresh disposable credentials, then deploy the OSS Task 0/Task 3 resolver changes. Record both deployed commits and image digests. Only after the auth incident is closed, land Cloud PR #603 and Tasks 1–2; verify a failed synthetic inference request can be retrieved by the same `x-bitrouter-request-id` and returns a complete ordered attempt list.

- [ ] **Step 2: Audit the two benchmark model chains**

For `moonshotai/kimi-k3` and `deepseek/deepseek-v4-pro`, first prove the matrix uses the admitted scored credential context, then run every Task 5 shape and inspect attempts. Record exact executable candidate count after credential/capability filters, actual attempted providers, outbound protocols, and content-free error classes. Treat vendor diagnosis as a new post-auth test; do not carry the prior run's 52 auth failures forward as provider evidence.

- [ ] **Step 3: Apply the first-party or third-party disposition**

- If no provider hop exists, fix the Cloud inbound adapter, route construction, admission, or post-hop rendering path and add the failing matrix row as a regression test.
- If all candidates share the same first-party protocol conversion failure, fix the canonical-to-provider adapter and retain the cross-provider regression fixture.
- If one provider alone fails, quarantine the exact provider/model/protocol/request-shape tuple, keep healthy fallbacks admitted, and send the vendor request IDs, UTC timestamps, protocol, shape flags, status/error class, and duration—never prompt content.
- If Kimi still has only one executable candidate, either provision and validate a second provider credential or remove Kimi from a production policy tier until the single candidate passes the entire matrix. A one-provider catalog entry is not accepted as resilient merely because a tiny sentinel passes.

- [ ] **Step 4: Apply third-party harness remediation before the next freeze**

For Harbor/Terminus, bake pinned `tmux` and `asciinema` into the runner image and preflight their exact versions. Pin/cache the LiteLLM cost map locally so a remote TLS/read failure is outside the setup critical path. Separate setup, agent, and verifier timeout accounting; choose the next frozen agent timeout from the completed baseline distribution and never change it after run admission.

For OpenAI Codex, retain the route because it succeeded 159 times, but classify timeout phase as connect, headers/TTFB, stream body decode, or idle. Permit bounded fallback/retry only before response commitment, preserve upstream request IDs in trusted evidence, and configure an alternate strong route before the next benchmark.

- [ ] **Step 5: Pass the full pre-scored acceptance gate**

Require all of the following:

1. a four-way refresh canary yields one success, three typed losers, and one live unrevoked successor family;
2. `bitrouter cloud whoami --check` reports usable credentials and its credential-context record exactly matches every scored daemon;
3. every mandatory model/shape/bucket row passes at concurrency four;
4. failed fixture requests expose complete Cloud attempt receipts;
5. injected transient failures create non-zero OSS reliability events and clamp only the matching shape to the certified baseline;
6. Kimi and DeepSeek each have either a passing executable fallback chain or are removed from the policy;
7. Harbor runner tools and offline cost map pass before identities are allocated;
8. frozen config, source commit, registry digest, auth-source class, authority identifier, timeouts, case list, and checksums are recorded.

- [ ] **Step 6: Re-run the 13-by-3 benchmark only after acceptance**

Run three trials in parallel with four live/launching case slots per unfinished trial whenever at least four cases remain. Do not retry/relaunch a started identity. Treat long cases as occupying one slot, keep frozen timeouts, reconcile receipts by correlated request ID, and fail closed on safety, identity, source, auth, quota, or settlement violations.

- [ ] **Step 7: Publish the final diagnostic comparison to PR #766**

Report old versus new gateway/timeout counts, provider-attempt distributions, reliability fallback counts by shape, runtime-valid cases, rewards with the same validity caveat, authoritative costs, failed logical request budget, and exact cleanup residue. Do not claim a vendor outage unless Task 1 receipts prove vendor-isolated failures.

---

## Completion Criteria

This program is complete only when:

- four concurrent refreshes leave exactly one usable, unrevoked successor family;
- independent OSS processes sharing one credential file perform one serialized refresh and re-read the rotated token;
- preflight, inference, and settlement prove the same content-free credential context;
- a local request ID resolves to its Cloud request and ordered provider attempts;
- terminal OAuth refresh failures are typed as auth/reauthentication failures rather than generic gateway failures;
- a shallow sentinel cannot close a complex tools/history circuit;
- OSS reliability events are non-zero under injected failures and policy fallback is keyed only by model plus structural shape;
- Cloud and OSS both preserve healthy fallbacks while isolating the exact failing tuple;
- the capability matrix passes before scored identities start;
- the rerun has authoritative settlement or fails closed with a typed, actionable reason;
- all Cloud and OSS resources are cleaned with zero tagged residue; and
- every commit, deployment, gate, result, and cleanup checkpoint is linked from PR #766.
