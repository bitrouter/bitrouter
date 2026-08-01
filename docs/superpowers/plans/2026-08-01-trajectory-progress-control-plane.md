# Persistent Trajectory State & Progress Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a durable, protocol-native, task-agnostic trajectory state layer that can detect operational trajectory inflation and let a signed policy lock conservatively hold or escalate model capability without relying on benchmark IDs, workflow names, private role headers, injected task metadata, or task-specific success labels.

**Architecture:** The named-policy path becomes async at pipeline Stage 0, after authentication. It resolves causal ancestry from native protocol IDs first and from canonical message-prefix digests second, then appends an immutable request-start event and reduces the persisted episode into a `TrajectorySnapshot` in one database transaction. A pure `ProgressGuard` combines that snapshot with the current source-independent route projection and an operator-signed lock clause to produce a durable `RouteIntent`. Settlement appends authoritative usage/outcome evidence and a transactional Eval Exchange outbox item. The same event reducer powers live routing, CLI inspection, and deterministic replay; raw prompt/task content is never persisted by this stage.

**Tech Stack:** Rust, Tokio, async-trait, HMAC-SHA-256, SeaORM/sea-orm-migration across SQLite/Postgres/MySQL, the existing canonical `Prompt` pipeline, policy-lock v2, Eval Exchange v1, Cargo nextest/test, Clippy, and rustfmt.

## Why This Is the Next Product Stage

The current branch already contains the generalized agent-trace projection, signed named-policy runtime, cost-aware compiler, counterfactual evaluator, Eval Exchange, fallback backoff, and the generalized `@auto` template. The remaining gap is not another route-key heuristic:

- `WorkflowIdentityTracker` is a process-local `Mutex<HashMap<...>>`; restart loses context epochs and it keys part of its diagnostic identity from benchmark/trial headers.
- `OnlineWorkflowState` classifies only the current canonical prompt. It has no durable causal record of previous route decisions, recoveries, cost, latency, or repeated low-capability selections.
- `PendingEvalDecisionStore` is process-local correlation state. A restart can lose an in-flight decision before settlement creates its Eval Exchange subject.
- Request-scope settlement captures cost/latency evidence, but there is no operational episode evaluator for turn inflation, repeated recovery, or time-to-outcome.
- The latest live benchmark evidence is decisive at the trajectory level even before exact cost reconciliation: a passing candidate took 72 model rounds where control took 19, and another trace exceeded the control trajectory after an early recovery because the next request immediately downgraded again. The generic defect is the absence of a progress/convergence guard, not the identity of either benchmark case.

The product response is therefore a persistent trajectory control plane. The benchmark trace is validation evidence only; no benchmark name, case number, task text, or model-specific exception may enter runtime behavior.

## Stage Scope

This single long-lived PR delivers the following vertical slice:

1. A versioned, owner-scoped, append-only trajectory ledger with explicit history completeness.
2. Protocol-native and canonical-prefix causal correlation that works without private BitRouter headers.
3. Deterministic operational health reduction across request count, elapsed time, repeated projection/tier streaks, recovery recurrence, context growth, tokens, and settled cost.
4. An optional lock-owned progress guard with conservative incomplete-history behavior and persisted escalation hold-down.
5. Durable request/decision/settlement correlation and an idempotent transactional outbox.
6. Built-in L1 operational episode evaluations published through Eval Exchange without inventing semantic pass/fail.
7. Explain, replay, retention, migration, privacy, cross-protocol, and benchmark-derived regression coverage.

## Explicit Non-Goals

- No task-native, benchmark-native, or LLM-judge semantic evaluator is required to route a request.
- HTTP success, absence of an exception, or a model's prose claim must not be converted into `quality.pass`.
- No prompt/task body, generated response text, secrets, tool arguments, file contents, or user-authored metadata is persisted by the trajectory ledger. Content equality uses an installation-keyed HMAC rather than a plain content hash.
- No `x-bitrouter-benchmark-*`, `x-bitrouter-trial-*`, `x-bitrouter-agent-role`, `x-superpowers-*`, workflow name, task ID, case ID, or private dataset field participates in correlation, health reduction, guard evaluation, or policy keys.
- No model name is hard-coded in the reducer or guard. Policy tiers and the capable/escalation set remain operator-owned lock data.
- No online mutation of the signed policy lock, learned per-task override, or benchmark-result injection is introduced.
- No remote control plane, Redis, new service, Python/Node runtime, or external evaluator is required.
- The legacy global `policy_table:` transform remains compatibility-only. Progress control is available only to authenticated named policies bound through `@preset`, where the signed lock and owner identity are available.

## Global Correctness Constraints

- Existing configs and locks without `progress_guard` retain byte-for-byte routing behavior and do not start trajectory persistence.
- A configured guard may only preserve or select a policy-declared capable tier; it must never downgrade the static/tool-safe decision.
- Tool-safety clamping remains the final capability floor after progress evaluation.
- Native causal IDs outrank digest inference. Conflicts become `Incomplete` and emit evidence; they are never silently resolved by caller-supplied benchmark/workflow headers.
- `Complete`, `Incomplete`, and `Unknown` are distinct states. Missing cost/token/outcome data stays absent, never coerced to zero.
- Every event is owner-scoped, schema-versioned, content-digested, ordered monotonically within its episode, and idempotent by event/request identity.
- Live routing and offline replay call the same pure reducer and guard functions.
- Any corrupt sequence, digest mismatch, cross-owner parent, or ancestry cycle fails closed for history-dependent downgrades and remains explainable.
- Follow `AGENTS.md`: no `#[allow]`, no public-module re-exports, no `.unwrap`/`.expect`/`panic!` in production code, no dead code, conventional commits under 60 characters.
- Every behavior change follows RED -> GREEN -> REFACTOR. Tests assert observable behavior and hand-derived values rather than source text.

---

## Task 1: Define the trajectory wire contracts and durable schema

**Files:**
- Create: `apps/bitrouter/src/trajectory/mod.rs`
- Create: `apps/bitrouter/src/trajectory/types.rs`
- Create: `apps/bitrouter/src/trajectory/store.rs`
- Create: `apps/bitrouter/src/db/migration/m20240101_000012_create_trajectory_ledger.rs`
- Modify: `apps/bitrouter/src/db/migration/mod.rs`
- Modify: `apps/bitrouter/src/lib.rs`
- Test: unit tests in the new modules and migration tests under `apps/bitrouter/src/db`

**Interfaces:**

```rust
pub const TRAJECTORY_SCHEMA_VERSION: u32 = 1;

pub enum HistoryCompleteness {
    Complete,
    Incomplete,
    Unknown,
}

pub enum TrajectoryEventKind {
    RequestStarted,
    RouteIntentRecorded,
    RequestSettled,
    GuardActivated,
    EpisodeClosed,
}

pub struct TrajectoryEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub owner_user_id: String,
    pub episode_id: String,
    pub request_id: Option<String>,
    pub sequence: u64,
    pub kind: TrajectoryEventKind,
    pub evidence: TrajectoryEvidence,
    pub captured_at: String,
}
```

The migration creates:

- `trajectory_episodes`: global `episode_id` primary key, owner, correlation digest/key ID/source, completeness, next sequence, first/last timestamps, optional close timestamp, and latest request ID.
- `trajectory_events`: immutable global `event_id` primary key, owner, episode, optional request ID, episode sequence, kind, canonical JSON, content digest, and timestamp; unique `(episode_id, sequence)`.
- `trajectory_requests`: global `request_id` primary key, owner, episode, start/settlement event IDs, canonical full-input digest, native parent ID when present, protocol, and status. This is an index over immutable events, not a second source of event truth.
- `trajectory_outbox`: global `outbox_id` primary key, owner, topic, canonical payload JSON/digest, attempts, created timestamp, and optional delivered timestamp.

- [x] **Step 1: Write failing migration tests.** Run migrations on SQLite memory, assert all four tables and unique indexes exist, rerun idempotently, then run `down` and prove only migration 12 objects disappear.
- [x] **Step 2: Write failing validation tests.** Reject unsupported versions, empty/bounded identifiers, invalid RFC3339 timestamps, mismatched event digests, request/episode owner mismatches, duplicate sequence numbers, cross-owner parents, and mutable replacement of an existing event ID.
- [x] **Step 3: Implement versioned Serde contracts and canonical SHA-256 event digests.** Persist only bounded structural attributes, numeric measures, categorical state, and digests. Task 1 treats correlation digests plus their key IDs as opaque, already-keyed values and must never accept or hash raw message content; Task 2 owns canonical message HMAC and installation-key lifecycle. Validation rejects credential-shaped attributes using the same policy as Eval Exchange.
- [x] **Step 4: Implement transaction-aware store methods.** Add `begin_request`, `append_route_intent`, `settle_request`, `events_for_episode`, `request`, `pending_outbox`, and `mark_outbox_delivered`. Each accepts an owner and enforces it in the query, not after loading.
- [x] **Step 5: Prove idempotency and atomicity.** Duplicate identical starts/settlements are no-ops; conflicting duplicates fail. A forced outbox insert failure rolls back settlement and episode-head updates.
- [x] **Step 6: Run `cargo test -p bitrouter --all-features trajectory::store` and migration tests until GREEN.**
- [x] **Step 7: Commit `feat(trajectory): add durable event ledger`.**

## Task 2: Correlate episodes from native protocol evidence

**Files:**
- Create: `apps/bitrouter/src/trajectory/correlation.rs`
- Create: `apps/bitrouter/src/trajectory/canonical.rs`
- Modify: `apps/bitrouter/src/paths.rs`
- Modify: `apps/bitrouter/Cargo.toml`
- Modify: root `Cargo.toml`
- Modify: `apps/bitrouter/src/workflow_state/online.rs`
- Modify: `crates/bitrouter-sdk/src/language_model/routing.rs`
- Modify: `crates/bitrouter-sdk/src/language_model/pipeline.rs`
- Modify: `crates/bitrouter-sdk/src/language_model/tests.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Test: unit tests in `correlation.rs`; integration tests in Task 7

**Interfaces:**

```rust
pub enum CorrelationSource {
    NativeParentId,
    CanonicalPrefix,
    ExplicitRoot,
    Unresolved,
}

pub struct CorrelationEvidence {
    pub native_parent_id: Option<String>,
    pub full_input_digest: String,
    pub ancestor_prefix_digests: Vec<String>,
    pub starts_with_prior_turns: bool,
}

#[async_trait]
pub trait ModelSelector: Send + Sync {
    async fn select_variant(
        &self,
        policy: &str,
        variant: Option<&str>,
        ctx: &mut PipelineContext,
    ) -> Result<()>;
}
```

- [ ] **Step 1: Write failing canonicalization tests.** Equivalent Chat Completions, Messages, and Responses histories produce the same ordered role/content-kind prefix digests. Provider-only metadata and task/workflow headers do not alter them; changing actual message ancestry does. Two installations produce different HMACs for identical text while one installation remains stable across restart.
- [ ] **Step 2: Write failing native-correlation tests.** A Responses `previous_response_id` resolves the exact prior BitRouter response/request. Native parent evidence wins over a conflicting prefix match; a cross-owner native ID is rejected and marks history incomplete.
- [ ] **Step 3: Write failing prefix-correlation tests.** For protocols without a parent ID, a later prompt containing a stored earlier full-input digest among its canonical prefix digests links to that request. A prompt that already contains assistant/tool history but has no provable ancestor starts a new incomplete episode. A genuine one-user-turn root starts complete.
- [ ] **Step 4: Create/load the private correlation key and make `ModelSelector` async.** Store a random key with restrictive permissions beside the installation ID, return a non-secret key ID for persisted records, await selectors in pipeline Stage 0, and update the counting selector/routing tests. Preserve the existing sync `PromptTransform` compatibility API; trajectory control does not run before authentication.
- [ ] **Step 5: Move named-policy decision recording into the async `PolicyRuntime`.** Use `PipelineContext::{request_id, caller, inbound_protocol, headers, prompt}` after Auth/Policy hooks. In one transaction, resolve/create the episode, append `RequestStarted`, and return the prior ordered events plus current correlation evidence.
- [ ] **Step 6: Stop using `WorkflowIdentityTracker` as causal state for named-policy decisions.** Existing adapter identity remains diagnostic in decision records; trajectory episode identity comes only from the correlation resolver. Add a negative test mutating benchmark, trial, workflow, agent-role, and Superpowers headers with no change in episode or route intent.
- [ ] **Step 7: Run focused SDK pipeline, policy-runtime, online-state, and correlation tests until GREEN.**
- [ ] **Step 8: Commit `refactor(policy): make selection trajectory-aware`.**

## Task 3: Reduce deterministic trajectory health and replay it

**Files:**
- Create: `apps/bitrouter/src/trajectory/health.rs`
- Create: `apps/bitrouter/src/trajectory/replay.rs`
- Modify: `apps/bitrouter/src/trajectory/types.rs`
- Modify: `apps/bitrouter/src/trajectory/store.rs`
- Test: unit/property-style table tests in `health.rs` and `replay.rs`

**Interfaces:**

```rust
pub struct TrajectoryHealth {
    pub completeness: HistoryCompleteness,
    pub request_count: u64,
    pub settled_request_count: u64,
    pub unsettled_request_count: u64,
    pub elapsed_ms: u64,
    pub same_projection_streak: u64,
    pub same_selected_tier_streak: u64,
    pub consecutive_unprotected_requests: u64,
    pub recovery_count: u64,
    pub requests_since_recovery: Option<u64>,
    pub context_growth_ppm: Option<u64>,
    pub total_tokens: Option<u64>,
    pub settled_cost_micro_usd: Option<u64>,
}

pub struct TrajectorySnapshot {
    pub episode_id: String,
    pub through_sequence: u64,
    pub health: TrajectoryHealth,
    pub active_hold_remaining: u64,
    pub evidence_digest: String,
}

pub fn reduce(events: &[TrajectoryEvent], protected_tiers: &BTreeSet<String>)
    -> Result<TrajectorySnapshot>;
```

- [ ] **Step 1: Write failing hand-derived reducer tests.** Cover a complete root, repeated projection, tier changes, recurring recovery, context growth, settled/unsettled interleaving, missing price/usage, failures, and a persisted guard hold. Assert every field from literal event sequences.
- [ ] **Step 2: Write failing corruption tests.** Reject gaps/duplicates in sequence, wrong episode/owner, settlement before start, two conflicting settlements, digest mismatch, close followed by new events, and arithmetic overflow.
- [ ] **Step 3: Implement the pure reducer.** Use checked/saturating conversions where wire widths differ. Recovery comes only from the existing generic `WorkflowStateKind::Recovery`/guarded projection evidence, never quoted failure text or task labels. Context growth compares canonical input size/token evidence, not content semantics.
- [ ] **Step 4: Implement deterministic replay.** `replay_episode(store, owner, episode_id)` loads events in sequence, validates every digest, calls `reduce`, and returns the same snapshot digest as the live begin/settle path.
- [ ] **Step 5: Add restart tests.** Start and settle several requests, drop all runtime objects, reconnect to the same SQLite file, and prove the next request sees the same health/hold state as an uninterrupted runtime.
- [ ] **Step 6: Run focused health/store/replay tests until GREEN.**
- [ ] **Step 7: Commit `feat(trajectory): reduce replayable health`.**

## Task 4: Add a signed, lock-owned progress guard

**Files:**
- Create: `apps/bitrouter/src/trajectory/guard.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Modify: `apps/bitrouter/src/policy_table_router.rs`
- Modify: `apps/bitrouter/src/workflow_state/decision.rs`
- Modify: `apps/bitrouter/src/policy_compile.rs`
- Test: unit tests in the files above

**Interfaces:**

```rust
pub struct ProgressGuardPolicy {
    pub escalation_tier: String,
    pub protected_tiers: BTreeSet<String>,
    pub max_consecutive_unprotected: Option<u64>,
    pub max_same_projection_unprotected: Option<u64>,
    pub max_recovery_count: Option<u64>,
    pub max_episode_requests: Option<u64>,
    pub max_episode_elapsed_ms: Option<u64>,
    pub max_episode_cost_micro_usd: Option<u64>,
    pub hold_for_requests: u64,
    pub incomplete_history: IncompleteHistoryAction,
}

pub enum IncompleteHistoryAction {
    Observe,
    Escalate,
}

pub struct RouteIntent {
    pub candidate_tier: Option<String>,
    pub selected_tier: Option<String>,
    pub clauses: Vec<RouteIntentClause>,
    pub trajectory_snapshot_digest: String,
    pub policy_digest: String,
}
```

- [ ] **Step 1: Write failing lock compatibility tests.** Old v1/v2 locks without `progress_guard` deserialize and serialize as before. The optional clause participates in the semantic digest, deterministic YAML, candidate diff, certificate validation, freeze/publish, reload, and rollback.
- [ ] **Step 2: Write failing guard validation tests.** Require a defined escalation tier, non-empty protected tiers containing the escalation tier, positive configured thresholds/hold length, and a named policy. Reject guard clauses on legacy global `policy_table:` input.
- [ ] **Step 3: Write failing pure guard tests.** Prove disabled guards preserve the candidate; protected tiers reset vulnerable streaks; a recovery request can trigger immediate escalation; repeated unprotected/same-projection requests trigger at exact boundaries; unknown cost cannot satisfy a cost threshold; `IncompleteHistoryAction::Escalate` is conservative; active hold persists for exactly N subsequent requests.
- [ ] **Step 4: Define precedence and non-downgrade invariants.** Compute static candidate, apply progress escalation/hold, then apply the existing tool-use floor. Record every applied/skipped clause. A guard-selected tier must be in `protected_tiers`; no clause may replace a protected/tool-safe decision with an unprotected tier.
- [ ] **Step 5: Persist intent before upstream execution.** Append `RouteIntentRecorded` and, when triggered, `GuardActivated` in the request-start transaction. Extend `PolicyDecisionReason` and JSONL records with progress reason, episode/sequence, completeness, health digest, candidate tier, selected tier, and clause IDs. JSONL remains diagnostic; the database event is authoritative.
- [ ] **Step 6: Make the compiler round-trip guard clauses.** Candidate generation may preserve or propose guard configuration only from explicit compiler input; L1 evidence must not silently mutate thresholds. Candidate diff/explain shows every guard change.
- [ ] **Step 7: Run lock, router, compiler, decision, reload, rollback, and guard tests until GREEN.**
- [ ] **Step 8: Commit `feat(policy): add trajectory progress guard`.**

## Task 5: Settle trajectories durably and publish L1 operational evaluation

**Files:**
- Create: `apps/bitrouter/src/trajectory/settlement.rs`
- Create: `apps/bitrouter/src/trajectory/evaluation.rs`
- Modify: `apps/bitrouter/src/eval/settlement.rs`
- Modify: `apps/bitrouter/src/eval/mod.rs`
- Modify: `apps/bitrouter/src/eval/admission.rs`
- Modify: `apps/bitrouter/src/eval/store.rs`
- Modify: `apps/bitrouter/src/assemble.rs`
- Modify: `crates/bitrouter-sdk/src/language_model/context.rs` only if a missing authoritative settlement field is required
- Test: unit and SQLite restart tests in trajectory/eval modules

**Operational metrics:**

- `trajectory.request_count`
- `trajectory.settled_request_count`
- `trajectory.elapsed_ms`
- `trajectory.same_projection_streak`
- `trajectory.same_selected_tier_streak`
- `trajectory.unprotected_streak`
- `trajectory.recovery_count`
- `trajectory.context_growth_ppm`
- `trajectory.total_tokens`
- `trajectory.cost.usd_micros`
- `trajectory.history_complete`
- Existing request metrics `cost.usd_micros` and `latency.ms`

- [ ] **Step 1: Write failing settlement tests.** A routed request's authoritative provider/model, usage, duration, error code, finish reason, and computed cost append exactly one `RequestSettled` event. Unknown price/usage remains absent. Duplicate settlement is idempotent; a conflict fails.
- [ ] **Step 2: Replace named-policy process-local correlation.** `EvalSettlementRecorder` loads the persisted request/intent by `(owner, request_id)`. Remove `PendingEvalDecisionStore` from `PolicyRuntime`; retain it only for the compatibility-only legacy transform until that path is removed.
- [ ] **Step 3: Build immutable episode-snapshot subjects.** Use `EvalScope::Episode`, `subject_id = episode_id`, and `eval_id = trajectory:<episode_id>:<through_sequence>`. Evidence contains only redacted event/snapshot digests and structural attributes. Requested dimensions exactly match available L1 metrics.
- [ ] **Step 4: Build an immediate built-in result.** Evaluator identity is `bitrouter.trajectory-operational` with `EvaluatorKind::Generic`; verdict is always `Inconclusive`; no `quality.pass` or hard violation is emitted. Credit only decisions present in the subject and only the metrics they influenced.
- [ ] **Step 5: Add a trusted built-in submission principal.** It is owner-scoped and may submit only `trajectory.*`, `cost.usd_micros`, and `latency.ms`. External authority admission remains unchanged.
- [ ] **Step 6: Make publication crash-safe.** The same transaction appends settlement and inserts a canonical outbox envelope. A bounded worker publishes subject/result idempotently, marks delivery only after admission succeeds, drains pending rows at startup, and drains on graceful shutdown. Restart tests cover crashes before publish and before delivery marking.
- [ ] **Step 7: Wire one shared `TrajectoryStore`/outbox publisher through `assemble.rs`.** Registration order must let Metering settle authoritative usage before trajectory evaluation consumes it, without making routing depend on asynchronous evaluator results.
- [ ] **Step 8: Run trajectory settlement, Eval Exchange, admission, compiler, metering, and assembly tests until GREEN.**
- [ ] **Step 9: Commit `feat(eval): publish trajectory operations`.**

## Task 6: Add retention, inspect, explain, and replay operations

**Files:**
- Modify: `crates/bitrouter-sdk/src/config/mod.rs`
- Modify: `crates/bitrouter-sdk/src/config/tests.rs`
- Modify: `apps/bitrouter/src/main.rs`
- Modify: `apps/bitrouter/src/output/reports/mod.rs`
- Create: `apps/bitrouter/src/output/reports/trajectory.rs`
- Modify: `docs/CLI.md`
- Modify: `skills/bitrouter/SKILL.md`
- Modify: `skills/bitrouter/references/cli.md`
- Test: config, CLI parser/report, and store pruning tests

**Configuration and CLI:**

```yaml
trajectory:
  enabled: true
  retention_days: 30
  outbox_batch_size: 100
```

```text
bitrouter trajectory inspect <episode-id> [--json]
bitrouter trajectory replay <episode-id> [--json]
bitrouter trajectory prune --before <RFC3339> [--dry-run]
```

- [ ] **Step 1: Write failing config tests.** Default `trajectory.enabled` is false. A lock containing `progress_guard` requires it true. Validate positive retention/batch bounds and schema output. Existing config fixtures remain valid unchanged.
- [ ] **Step 2: Write failing CLI parser/report tests.** Inspect displays correlation source, completeness, current health, active hold, route intents, and event digests. Replay displays live/replayed digest equality or the exact first corrupt event. JSON output is stable and contains no raw task content.
- [ ] **Step 3: Implement owner-safe pruning.** Delete delivered outbox rows and closed/expired episode indexes/events in bounded transactions. Never prune pending outbox work. `--dry-run` reports exact counts without mutation.
- [ ] **Step 4: Redact at write time, not display time.** Add tests with API keys, bearer tokens, tool arguments, file bodies, prompt text, and private metadata; none may appear in event JSON, Eval evidence, CLI output, or logs. Digest equality remains usable for ancestry.
- [ ] **Step 5: Document enablement, guarantees, incomplete-history semantics, metric meaning, replay, and operational recovery. Update the bundled skill because CLI/config/wiring changed.**
- [ ] **Step 6: Run config, CLI, docs/examples, skill, and pruning tests until GREEN.**
- [ ] **Step 7: Commit `feat(cli): operate trajectory history`.**

## Task 7: Prove cross-protocol generality and the inflation regression

**Files:**
- Create: `apps/bitrouter/tests/trajectory_progress_control.rs`
- Create: `apps/bitrouter/tests/fixtures/trajectory/complete_chat.jsonl`
- Create: `apps/bitrouter/tests/fixtures/trajectory/complete_messages.jsonl`
- Create: `apps/bitrouter/tests/fixtures/trajectory/complete_responses.jsonl`
- Create: `apps/bitrouter/tests/fixtures/trajectory/recovery_then_repeat.jsonl`
- Modify: `apps/bitrouter/tests/workflow_state_replay.rs`
- Modify: `apps/bitrouter/tests/workflow_state_real_agent_e2e.rs` only where shared setup is reusable
- Modify: `templates/auto-router/bitrouter.yaml`
- Modify: `templates/auto-router/policy-lock.yaml`
- Modify: `templates/auto-router/README.md`

- [ ] **Step 1: Write a failing HTTP matrix with no private routing headers.** Equivalent multi-turn Chat Completions, Messages, and Responses requests must form one episode per conversation, reduce to equivalent health, and preserve protocol diagnostics without changing the guard decision.
- [ ] **Step 2: Test incomplete and conflicting history.** Truncated histories, unknown native parents, cross-user parent IDs, compaction, interleaved conversations, duplicated retries, and restarts produce explicit deterministic completeness and never share owner state.
- [ ] **Step 3: Add the benchmark-derived invariant fixture.** Use a synthetic task-neutral sequence: opening -> recovery -> repeated review/context requests whose static candidate is unprotected. With guard disabled, preserve current routing. With guard enabled, recovery immediately selects the protected tier and hold-down prevents the next request from bouncing back. Assert a configured maximum unprotected streak can never be exceeded.
- [ ] **Step 4: Prove input independence.** Replace fixture task text, tool names, model IDs, harness/user-agent, workflow names, case labels, and benchmark headers while preserving structural ancestry/projections; the health thresholds and route intent remain identical.
- [ ] **Step 5: Opt the generalized `@auto` template into trajectory persistence and an explicitly documented conservative guard only after the synthetic matrix is green. Template thresholds are policy examples, not hidden runtime defaults. Existing user locks remain unchanged.**
- [ ] **Step 6: Run restart, cross-protocol, policy reload/rollback, Eval outbox, real-agent, and replay integration suites until GREEN.**
- [ ] **Step 7: Commit `test(trajectory): prove progress control`.**

## Task 8: Validate the exact PR tree and record benchmark evidence

**Files:**
- Modify: this plan's checkboxes as tasks complete
- Modify: the single Draft PR description/checklist throughout execution
- Modify: implementation/docs only for defects found by validation

- [ ] **Step 1: Run focused tests after every task and record the command/result in the PR.**
- [ ] **Step 2: Run `cargo fmt -- --check`.**
- [ ] **Step 3: Run `cargo clippy --all-features --all-targets -- -D warnings`.**
- [ ] **Step 4: Run `cargo nextest run --all-features`; if nextest is unavailable, run `cargo test --all-features` and record the substitution.**
- [ ] **Step 5: Run `cargo run -p dist-helper -- check`.**
- [ ] **Step 6: Re-run the benchmark scenarios as external validation.** Compare task outcome, request/turn count, time-to-outcome, exact authoritative cost, recovery recurrence, and model-tier sequence against control and the previous branch. Do not promote benchmark identifiers into product fixtures or policy keys.
- [ ] **Step 7: Classify the result honestly.** A semantic regression blocks readiness. A pass with unbounded turn/cost inflation also blocks readiness. Missing authoritative cost remains `unknown`; it is not zero and does not erase a decisive trajectory-count regression.
- [ ] **Step 8: Update the PR description with final scope, commit map, test evidence, benchmark evidence, remaining risks, compatibility notes, and rollout/rollback instructions. Mark ready for review only when every required gate is green.**
- [ ] **Step 9: Commit final validation/docs fixes using a scoped conventional title; do not create a second PR.**

## Requirement Coverage

| Requirement | Primary tasks | Executable evidence |
|---|---|---|
| Durable cross-request state | 1-3 | migration, idempotency, restart, and replay tests |
| No task-data injection | 2, 6, 7 | negative header/content-independence and redaction tests |
| Protocol-native causal history | 2, 7 | native parent and canonical-prefix HTTP matrix |
| Honest incomplete history | 1-3, 7 | conflict/truncation/completeness tests |
| Generic progress/convergence risk | 3, 4 | pure reducer and threshold-boundary tables |
| No immediate downgrade after recovery | 4, 7 | persisted hold-down regression fixture |
| Policy ownership and auditability | 4 | signed lock digest/diff/reload/rollback tests |
| Operational evaluation, no fake semantics | 5 | Inconclusive L1 Eval Exchange subject/result tests |
| Crash-safe correlation/publication | 1, 2, 5 | transaction rollback and restart/outbox tests |
| Explainable deterministic decisions | 3, 4, 6 | live/replay digest and RouteIntent clause equality |
| Privacy and retention | 1, 6 | write-time secret-content redaction and bounded prune tests |
| Existing behavior compatibility | 4, 6, 7 | guard-disabled legacy lock/config matrix |

## Single-PR Delivery Protocol

1. The first commit contains this reviewed phase plan and immediately creates one Draft PR stacked on `codex/policy-effect-v2-short13`.
2. Every task lands as one or more scoped conventional commits on the same branch and same PR. No task creates a separate PR.
3. After every push, update the PR checklist, current implementation status, test evidence, and any changed risk/scope. The PR description is the live source of progress truth; this document is the detailed execution contract.
4. If research or implementation invalidates an assumption, update this plan and the PR description in the same commit that changes direction. Do not silently drift.
5. The concurrent benchmark remains external evidence. Its results are copied into the PR's validation section once authoritative; the benchmark worktree and running session are not modified from this branch.
6. Keep the PR Draft until the full validation and benchmark gates are satisfied. A passing task with material trajectory/cost inflation is not sufficient for readiness.
