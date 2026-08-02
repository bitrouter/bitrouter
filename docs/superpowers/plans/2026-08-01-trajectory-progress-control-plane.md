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

- [x] **Step 1: Write failing canonicalization tests.** Equivalent Chat Completions, Messages, and Responses histories produce the same ordered role/content-kind prefix digests. Provider-only metadata and task/workflow headers do not alter them; changing actual message ancestry does. Two installations produce different HMACs for identical text while one installation remains stable across restart.
- [x] **Step 2: Write failing native-correlation tests.** A Responses `previous_response_id` resolves the exact prior BitRouter response/request. Native parent evidence wins over a conflicting prefix match; a cross-owner native ID is rejected and marks history incomplete.
- [x] **Step 3: Write failing prefix-correlation tests.** For protocols without a parent ID, a later prompt containing a stored earlier full-input digest among its canonical prefix digests links to that request. A prompt that already contains assistant/tool history but has no provable ancestor starts a new incomplete episode. A genuine one-user-turn root starts complete.
- [x] **Step 4: Create/load the private correlation key and make `ModelSelector` async.** Store a random key with restrictive permissions beside the installation ID, return a non-secret key ID for persisted records, await selectors in pipeline Stage 0, and update the counting selector/routing tests. Preserve the existing sync `PromptTransform` compatibility API; trajectory control does not run before authentication.
- [x] **Step 5: Move named-policy decision recording into the async `PolicyRuntime` behind an optional trajectory runtime.** Use `PipelineContext::{request_id, caller, inbound_protocol, headers, prompt}` after Auth/Policy hooks. In one transaction, resolve/create the episode, append `RequestStarted`, and return the prior ordered events plus current correlation evidence. Task 2 tests explicitly inject this runtime, while production assembly continues to pass `None`; Task 4/6 own guard/config activation so existing named policies cannot begin persistence early.
- [x] **Step 6: Stop using `WorkflowIdentityTracker` as causal state for named-policy decisions.** Existing adapter identity remains diagnostic in decision records; trajectory episode identity comes only from the correlation resolver. Add a negative test mutating benchmark, trial, workflow, agent-role, and Superpowers headers with no change in episode or route intent.
- [x] **Step 7: Run focused SDK pipeline, policy-runtime, online-state, and correlation tests until GREEN.**
- [x] **Step 8: Commit `refactor(policy): make selection trajectory-aware`.**
- [x] **Review fix round 1/5:** replace representation-level metadata stripping with typed canonicalization, harden installation identity/key publication, make exact retries immutable and correlation appends CAS-safe, validate episode heads before mutation, treat ambiguous prefixes as unresolved, and map only malformed native-parent evidence to HTTP 400.
- [x] **Review fix round 2/5:** preserve semantic message boundaries while normalizing homogeneous tool artifacts, fail closed on corrupt key material with secret-derived key identity, persist keyed native-parent evidence for exact retries, reload deterministic concurrent start winners, keep `latest_request_id` tied to `RequestStarted`, and classify retryable database errors by typed codes with bounded backoff.
- [x] **Review fix round 3/5:** emit native-parent evidence as a key-bound `hmac-sha256:<key-id>:<digest>` token while retaining SHA-only outbox payloads, split trusted native links across correlation-key epochs without copying prior history, make exact retries depend only on immutable persisted evidence, and remove the remaining production `expect` from canonical turn merging.

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

**Task 3 reducer evidence contract:** The reducer reads only the following
exact, bounded, namespaced attributes and never parses prose, task labels,
model names, headers, or arbitrary metadata:

- `RequestStarted`: categorical `history.completeness` and
  `correlation.source`, plus structural `request.canonical_input_bytes`.
  Task 2's typed canonicalizer supplies the byte count of the exact canonical
  JSON bytes used as the full-input HMAC input; only the count is persisted.
  There is no model-token estimate and no content persistence.
- `RouteIntentRecorded`: categorical `route.projection`,
  `route.selected_tier`, and `route.workflow_state`. Projection and workflow
  state must be valid generic agent-trace values and agree. Recovery is only
  the exact `recovery` workflow-state value.
- `RequestSettled`: structural `settlement.total_tokens` and
  `settlement.cost_micro_usd`. Presence is authoritative, including an
  explicit zero; absence remains unknown.
- `GuardActivated`: structural `guard.hold_for_requests`, a positive value no
  greater than `u32::MAX`.

Completeness folds all starts and required correlation/intent evidence with
`Incomplete` dominating `Unknown`, which dominates `Complete`; missing
required evidence contributes `Unknown`. Token and cost totals are `None` when
there are no settled requests or if any settled request lacks the corresponding
value; otherwise checked sums produce `Some`, including `Some(0)`. Streaks advance only on route-intent
events. A newly typed route intent persists the exact structural fact
`route.selected_is_protected` from the signed policy active for that decision,
so later policy reloads cannot reinterpret history. Legacy route evidence still
uses exact membership in the caller-supplied set; a missing tier resets the
unprotected streak and contributes unknown completeness. A hold event leaves its full value remaining and each subsequent
request-start consumes exactly one. Context growth compares the first and latest
`RequestStarted` endpoints: a missing endpoint or zero first value yields
`None`, missing middle values do not matter, shrinkage yields zero, and
multiplication overflow is an error.

Replay is explicitly `replay_episode(store, owner, episode_id,
protected_tiers)`: protected-tier policy data affects the snapshot and cannot
be hidden in global state. Store loading and the reducer validate the event
index/digest, sequence, owner, and episode before producing the same canonical
SHA-256 snapshot digest as direct reduction; that digest excludes its own
field.

- [x] **Step 1: Write failing hand-derived reducer tests.** Cover a complete root, repeated projection, tier changes, recurring recovery, context growth, settled/unsettled interleaving, missing price/usage, failures, and a persisted guard hold. Assert every field from literal event sequences.
- [x] **Step 2: Write failing corruption tests.** Reject gaps/duplicates in sequence, wrong episode/owner, settlement before start, two conflicting settlements, digest mismatch, close followed by new events, and arithmetic overflow.
- [x] **Step 3: Implement the pure reducer.** Use checked/saturating conversions where wire widths differ. Recovery comes only from the existing generic `WorkflowStateKind::Recovery`/guarded projection evidence, never quoted failure text or task labels. Context growth compares canonical input size/token evidence, not content semantics.
- [x] **Step 4: Implement deterministic replay.** `replay_episode(store, owner, episode_id, protected_tiers)` loads events in sequence, validates every digest, calls `reduce`, and returns the same snapshot digest as the live begin/settle path. The explicit tier set is required because protected classification is policy input and replay must not depend on hidden global state.
- [x] **Step 5: Add restart tests.** Start and settle several requests, drop all runtime objects, reconnect to the same SQLite file, and prove the next request sees the same health/hold state as an uninterrupted runtime.
- [x] **Step 6: Run focused health/store/replay tests until GREEN.**
- [x] **Step 7: Commit `feat(trajectory): reduce replayable health`.**
- [x] **Review fix round 1/5:** enforce the strict per-request phase machine,
  endpoint-presence and settlement-total semantics, transactional store/reducer
  parity, full-event exact retries, and bounded typed contention handling across
  route, guard, and settlement appends.
- [x] **Review fix round 2/5:** validate a brand-new episode's sequence-one
  start through the reducer before any mutation, require its indexed
  completeness/source/key-epoch facts to agree with immutable start evidence,
  and prove invalid or divergent starts roll back without residual rows while
  valid incomplete/unmatched starts remain replayable.

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

- [x] **Step 1: Write failing lock compatibility tests.** Old v1/v2 locks without `progress_guard` deserialize and serialize as before. The optional clause participates in the semantic digest, deterministic YAML, candidate diff, certificate validation, freeze/publish, reload, and rollback.
- [x] **Step 2: Write failing guard validation tests.** Require a defined escalation tier, non-empty protected tiers containing the escalation tier, positive configured thresholds/hold length, and a named policy. Reject guard clauses on legacy global `policy_table:` input.
- [x] **Step 3: Write failing pure guard tests.** Prove disabled guards preserve the candidate; protected tiers reset vulnerable streaks; a recovery request can trigger immediate escalation; repeated unprotected/same-projection requests trigger at exact boundaries; unknown cost cannot satisfy a cost threshold; `IncompleteHistoryAction::Escalate` is conservative; active hold persists for exactly N subsequent requests.
- [x] **Step 4: Define precedence and non-downgrade invariants.** Compute static candidate, apply progress escalation/hold, then apply the existing tool-use floor. Record every applied/skipped clause. A guard-selected tier must be in `protected_tiers`; no clause may replace a protected/tool-safe decision with an unprotected tier.
- [x] **Step 5: Persist intent before upstream execution.** Append `RouteIntentRecorded` and, when triggered, `GuardActivated` in the request-start transaction. Extend `PolicyDecisionReason` and JSONL records with progress reason, episode/sequence, completeness, health digest, candidate tier, selected tier, and clause IDs. JSONL remains diagnostic; the database event is authoritative.
- [x] **Step 6: Make the compiler round-trip guard clauses.** Candidate generation may preserve or propose guard configuration only from explicit compiler input; L1 evidence must not silently mutate thresholds. Candidate diff/explain shows every guard change.
- [x] **Step 7: Run lock, router, compiler, decision, reload, rollback, and guard tests until GREEN.**
- [x] **Step 8: Commit `feat(policy): add trajectory progress guard`.**

## Task 5: Settle trajectories durably and publish L1 operational evaluation

**Files:**
- Create: `apps/bitrouter/src/trajectory/settlement.rs`
- Create: `apps/bitrouter/src/trajectory/evaluation.rs`
- Create: `apps/bitrouter/src/trajectory/publisher.rs`
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

- [x] **Step 1: Write failing settlement tests.** A routed request's authoritative provider/model, usage, duration, error code, finish reason, and computed cost append exactly one `RequestSettled` event. Unknown price/usage remains absent. Duplicate settlement is idempotent; a conflict fails.
- [x] **Step 2: Replace named-policy process-local correlation.** `EvalSettlementRecorder` loads the persisted request/intent by `(owner, request_id)`. Remove `PendingEvalDecisionStore` from `PolicyRuntime`; retain it only for the compatibility-only legacy transform until that path is removed.
- [x] **Step 3: Build immutable episode-snapshot subjects.** Use `EvalScope::Episode`, `subject_id = episode_id`, and `eval_id = trajectory:<episode_id>:<through_sequence>`. Evidence contains only redacted event/snapshot digests and structural attributes. Requested dimensions exactly match available L1 metrics.
- [x] **Step 4: Build an immediate built-in result.** Evaluator identity is `bitrouter.trajectory-operational` with `EvaluatorKind::Generic`; verdict is always `Inconclusive`; no `quality.pass` or hard violation is emitted. Credit only decisions present in the subject and only the metrics they influenced.
- [x] **Step 5: Add a trusted built-in submission principal.** It is owner-scoped and may submit only `trajectory.*`, `cost.usd_micros`, and `latency.ms`. External authority admission remains unchanged.
- [x] **Step 6: Make publication crash-safe.** The same transaction appends settlement and inserts a canonical outbox envelope. A bounded worker publishes subject/result idempotently, marks delivery only after admission succeeds, drains pending rows at startup, and drains on graceful shutdown. Restart tests cover crashes before publish and before delivery marking.
- [x] **Step 7: Wire one shared `TrajectoryStore`/outbox publisher through `assemble.rs`.** Registration order must let Metering settle authoritative usage before trajectory evaluation consumes it, without making routing depend on asynchronous evaluator results.
- [x] **Step 8: Run trajectory settlement, Eval Exchange, admission, compiler, metering, and assembly tests until GREEN.**
- [x] **Step 9: Commit `feat(eval): publish trajectory operations`.**

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

- [x] **Step 1: Write failing config tests.** Default `trajectory.enabled` is false. A lock containing `progress_guard` requires it true. Validate positive retention/batch bounds and schema output. Existing config fixtures remain valid unchanged.
- [x] **Step 2: Write failing CLI parser/report tests.** Inspect displays correlation source, completeness, current health, active hold, route intents, and event digests. Replay displays live/replayed digest equality or the exact first corrupt event. JSON output is stable and contains no raw task content.
- [x] **Step 3: Implement owner-safe pruning.** Delete delivered outbox rows and closed/expired episode indexes/events in bounded transactions. Never prune pending outbox work. `--dry-run` reports exact counts without mutation.
- [x] **Step 4: Redact at write time, not display time.** Add tests with API keys, bearer tokens, tool arguments, file bodies, prompt text, and private metadata; none may appear in event JSON, Eval evidence, CLI output, or logs. Digest equality remains usable for ancestry.
- [x] **Step 5: Document enablement, guarantees, incomplete-history semantics, metric meaning, replay, and operational recovery. Update the bundled skill because CLI/config/wiring changed.**
- [x] **Step 6: Run config, CLI, docs/examples, skill, and pruning tests until GREEN.**
- [x] **Step 7: Commit `feat(cli): operate trajectory history`.**

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

`workflow_state_real_agent_e2e.rs` is intentionally unchanged: its shared setup
launches installed agent CLIs and protocol-specific streaming mocks, while this
task's deterministic matrix needs direct HTTP requests, a file-backed ledger,
owner-scoped inspection, and restart control. Reusing that harness would make
the general progress invariant depend on external agent installations.

- [x] **Step 1: Write a failing HTTP matrix with no private routing headers.** Equivalent multi-turn Chat Completions, Messages, and Responses requests must form one episode per conversation, reduce to equivalent health, and preserve protocol diagnostics without changing the guard decision.
- [x] **Step 2: Test incomplete and conflicting history.** Truncated histories, unknown native parents, cross-user parent IDs, compaction, interleaved conversations, duplicated retries, and restarts produce explicit deterministic completeness and never share owner state.
- [x] **Step 3: Add the benchmark-derived invariant fixture.** Use a synthetic task-neutral sequence: opening -> recovery -> repeated review/context requests whose static candidate is unprotected. With guard disabled, preserve current routing. With guard enabled, recovery immediately selects the protected tier and hold-down prevents the next request from bouncing back. Assert a configured maximum unprotected streak can never be exceeded.
- [x] **Step 4: Prove input independence.** Replace fixture task text, tool names, model IDs, harness/user-agent, workflow names, case labels, and benchmark headers while preserving structural ancestry/projections; the health thresholds and route intent remain identical.
- [x] **Step 5: Opt the generalized `@auto` template into trajectory persistence and an explicitly documented conservative guard only after the synthetic matrix is green. Template thresholds are policy examples, not hidden runtime defaults. Existing user locks remain unchanged.**
- [x] **Step 6: Run restart, cross-protocol, policy reload/rollback, Eval outbox, real-agent, and replay integration suites until GREEN.**
- [x] **Step 7: Commit `test(trajectory): prove progress control`.**

## Task 8: Reconcile native and canonical ancestry evidence

**Files:**
- Modify: `apps/bitrouter/src/trajectory/store.rs`
- Modify: `apps/bitrouter/src/trajectory/correlation.rs`
- Modify: `apps/bitrouter/src/trajectory/guard.rs`
- Modify: `apps/bitrouter/src/trajectory/replay.rs`

**Interfaces:** Native parent identity remains authoritative for episode selection. Canonical-prefix resolution is nevertheless evaluated independently; a unique different episode or ambiguous prefix sets `correlation.prefix_conflict = 1` and monotonically folds the chosen native episode to `Incomplete`. No caller-supplied metadata resolves the conflict.

- [x] **Step 1: Write the failing contradiction tests.** Extend `native_parent_resolves_exact_request_and_outranks_conflicting_prefix` so the native episode still wins while `HistoryCompleteness::Incomplete`, `history.completeness = incomplete`, and `correlation.prefix_conflict = 1` are observable. Add matching-native-prefix and ambiguous-prefix controls.
- [x] **Step 2: Run `cargo test -p bitrouter --all-features trajectory::correlation::tests::native_parent -- --nocapture`.** Expected RED: the conflicting request is currently `Complete` with no conflict marker.
- [x] **Step 3: Resolve prefix evidence alongside the trusted native parent in the existing transaction.** Preserve native selection; mark only contradictory or ambiguous non-empty prefix evidence incomplete. `PrefixResolution::None` remains acceptable for native delta/compacted Responses input.
- [x] **Step 4: Prove monotonic replay and guard behavior.** Reconnect to file-backed SQLite, replay the episode, and assert the same incomplete snapshot digest. With `incomplete_history: escalate`, assert a protected route; with `observe`, assert no invented semantic result.
- [x] **Step 5: Run focused correlation, store, replay, and guard tests until GREEN.**
- [x] **Step 6: Commit `fix(trajectory): expose ancestry conflicts`.**

## Task 9: Restore request Eval compatibility outside guarded trajectories

**Files:**
- Modify: `apps/bitrouter/src/assemble.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Modify: `apps/bitrouter/src/policy_table_router.rs`
- Modify: `apps/bitrouter/src/eval/settlement.rs`
- Test: integration tests in `apps/bitrouter/src/assemble.rs`

**Interfaces:** `PolicyRuntime::new` again consumes the same cloneable `PendingEvalDecisionStore` used by `EvalSettlementRecorder`. Routers without `progress_guard` call `with_eval_observer`; guarded routers retain metadata-only observation because trajectory settlement owns their episode evaluation and must not leak duplicate pending entries.

- [x] **Step 1: Write two failing assembled-App tests.** Send a settled named-policy request with `trajectory.enabled: false`, then with trajectory enabled but no policy guard. In both cases assert exactly one request-scope Eval subject with the selected/baseline tier and policy digest.
- [x] **Step 2: Add a guarded control.** A guarded request must emit the trajectory episode evaluation only, with no duplicate request subject and no retained pending decision after settlement.
- [x] **Step 3: Run the three tests and capture RED.** Expected RED: both unguarded configurations contain zero request Eval subjects.
- [x] **Step 4: Restore the shared pending store through assembly and runtime construction.** Select observer-with-pending only for unguarded named policies; preserve metadata for guarded policies and reload snapshots.
- [x] **Step 5: Run assembled-App, policy-lock, policy-table-router, and Eval settlement tests until GREEN.**
- [x] **Step 6: Commit `fix(eval): preserve unguarded policy subjects`.**

## Task 10: Terminally settle post-decision routing failures

**Files:**
- Modify: `crates/bitrouter-sdk/src/language_model/pipeline.rs`
- Modify: `crates/bitrouter-sdk/src/language_model/tests.rs`
- Modify: `apps/bitrouter/src/trajectory/settlement.rs`
- Modify: `apps/bitrouter/src/trajectory/store.rs`
- Test: guarded integration tests in `apps/bitrouter/tests/trajectory_progress_control.rs`

**Interfaces:** After Stage 1 succeeds, any Stage-2 error that occurs after a model selector may have durably recorded a route intent. Both streaming and non-streaming paths therefore call the existing `run_settlement(ctx, false, Some(error))` before returning because no response stream has opened yet. Unknown provider usage remains `UsageOrigin::Unknown` with absent token/cost evidence; it is never coerced to zero. Here, absent evidence means the typed metering event, trajectory event/outbox/evaluation, and final charge omit unknown facts; legacy non-null metering columns retain compatibility placeholders guarded by `UsageOrigin::Unknown` and `ChargeStatus::Unknown` and are never authoritative computed evidence.

**Review fix:** Exact retries reuse the first verified terminal trajectory settlement, including after a file-SQLite restart, without rebuilding it from retry-local duration. Reuse fails closed unless request status, terminal event index/content, request identity, and outbox index/content remain mutually consistent; direct store APIs still conflict on changed authoritative settlements. Machine-wide budget enforcement also fails closed on any unknown charge row instead of consuming its legacy zero placeholder as known spend.
The reused evaluation authority must exactly equal the operational envelope rebuilt from the persisted episode prefix through the indexed settlement, so a structurally valid envelope from another episode fails closed before publication.

- [x] **Step 1: Write SDK RED tests for no-route and failing route-hook paths in both execution modes.** Assert every registered settlement recorder runs once with the original error and unknown usage, while pre-request denials still do not fabricate a routed settlement.
- [x] **Step 2: Write a guarded App RED test.** Use a signed policy tier whose non-empty provider-qualified model has no routing-table entry. Assert the request currently remains `started` to prove the defect.
- [x] **Step 3: Settle Stage-2 failures in both pipeline entrypoints.** Preserve observe ordering: settlement phase and failed request-end occur exactly once.
- [x] **Step 4: Prove durable closure.** Assert `RequestStatus::Failed`, one settlement event/outbox record, no numeric token/cost facts, idempotent restart/reconciliation, successful publication, and prune eligibility after the cutoff.
- [x] **Step 5: Run SDK pipeline, metering, trajectory settlement/store, publisher, restart, and prune tests until GREEN.**
- [x] **Step 6: Commit `fix(pipeline): settle routing failures`.**

## Task 11: Preserve Responses continuation through gateway identity

**Files:**
- Modify: `crates/bitrouter-sdk/src/language_model/{builder,pipeline,types,context,settlement}.rs`
- Modify: built-in protocol decoders, `protocol/responses.rs`, and the HTTP server
- Create: `apps/bitrouter/src/continuation.rs`
- Modify: `apps/bitrouter/src/{assemble,paths}.rs`
- Modify: `apps/bitrouter/src/trajectory/{correlation,settlement,store}.rs`
- Modify: app database migrations
- Modify: `crates/bitrouter-observe/src/otel/exporter.rs`
- Test: SDK protocol/context tests, trajectory store/correlation/migration tests
- Modify: `apps/bitrouter/tests/trajectory_progress_control.rs`

**Interfaces:** Every Responses result encodes the bounded gateway `request_id` as a canonical reserved `brc_<base64url>` public continuation ID for streaming, non-streaming, native, and cross-protocol execution. Responses ingress validates the request id before the pipeline, so invalid or overlong custom ids return HTTP 400 without upstream or durable side effects. A generic, always-available continuation registry maps the owner-scoped reserved identity to the final provider-issued Responses ID using authenticated encryption; only ciphertext and nonce are durable. The authenticated authority record binds provider, account, protocol, effective API endpoint, auth scheme, and credential fingerprint, deliberately excluding service/model so signed policy may move tiers on the same authority. After ordinary model/policy selection, a mapped continuation retains only a matching-authority Responses target already in that selected chain, substitutes the decrypted native ID only for that target, and disables fallback. It never resurrects an out-of-chain historical service target. Missing, corrupt, expired, or changed authorities fail closed. Non-reserved unknown ids retain generic legacy-native compatibility and pin the first selected Responses target. Malformed reserved ids fail closed. Unmapped roots retain normal routing and fallback. The continuation commit is a required finalizer whose failure propagates before a successful terminal is emitted. Trajectory remains optional, strictly decodes the reserved ID to the original request identity, and stores no response alias.

**Review correction:** Forwarded native `ResponseStarted` parts from multiple server-tool rounds are provider metadata, not client lifecycle identity. The public gateway ID is known before the first frame, while request-local state retains the last clean final-round native ID and target. A registry row is published only after a successful terminal, never after an intermediate round, disconnect, abort, or upstream error. Registry keys are created lazily with private atomic-file semantics, corruption and unsupported key versions fail closed, rows have explicit expiry and bounded pruning, and concurrent/replayed gateway IDs are idempotent only for the same authenticated binding and ciphertext plaintext.

- [x] **Step 1: Write protocol RED tests and preserve typed provenance.** Decode native `response.created`, retain source-protocol metadata request-locally, and prove cross-protocol metadata cannot become a provider continuation.
- [x] **Step 2: Capture and correct the first native-identity attempt.** The original gateway-only encoder had no provider mapping; the native-ID encoder became inconsistent when a server-tool loop produced more than one provider response.
- [x] **Step 3: Write gateway-lifecycle RED tests.** Assert streaming and non-streaming Responses use the canonical reserved gateway identity on native, cross-protocol, missing-start, error, and multi-round server-tool paths while the final native ID remains request-local critical-finalizer input.
- [x] **Step 4: Write continuation registry RED tests.** Cover owner isolation, AEAD round trip, tamper/corruption/key-version failure, authority/account/protocol/endpoint binding, same-authority service changes, expiry/pruning, restart, concurrent insertion, exact idempotency, collision, and absence of plaintext provider IDs from every durable row.
- [x] **Step 5: Implement the always-enabled registry and lazy private key.** Add a dedicated migration/table, domain-separated owner lookup identity, versioned AEAD using the repository-locked crypto stack, bounded retention, and startup/opportunistic prune. Remove the trajectory `response_alias_id` column/index/binding/lookup and its shared HMAC namespace entirely.
- [x] **Step 6: Write route/executor RED tests.** Resolve a mapped reserved `previous_response_id` after ordinary model/policy selection, retain only a matching authority in the selected chain, install a typed request extension, rewrite only that selected Responses target's outbound body, and prove no fallback occurs. Non-reserved ids pass through as generic legacy-native ids; malformed/missing/expired reserved ids and unavailable/changed authorities fail closed.
- [x] **Step 7: Add the critical finalizer path.** Required finalizers run before a non-streaming success response and before a streaming success terminal is yielded; database/key-epoch errors propagate instead of being swallowed. Persist only a clean final native Responses ID and final serving authority. Disconnect, abort, upstream error, cross-protocol output, and intermediate tool rounds never publish a row.
- [x] **Step 8: Strengthen assembled HTTP continuation.** A stateful provider oracle rejects unissued native IDs. Prove reserved gateway ID stability, actual multi-round server-tool final-ID rewrite, restart and immediate post-terminal availability, authority-pinned/no-fallback routing without overriding policy tier changes, enabled/disabled trajectory equivalence, one complete episode when enabled, invalid-ingress zero side effects, and no plaintext native ID in ledger/outbox/Eval/registry/loggable records.
- [x] **Step 9: Run Responses protocol/context/pipeline, continuation/key/migration, trajectory correlation, assembled HTTP restart/privacy/retention, formatting, Clippy, and full nextest verification until GREEN.**
- [x] **Step 10: Commit a signed follow-up `fix(responses): persist gateway continuation` without amending or resetting the two earlier Task 11 commits.**
- [x] **Step 11: Reproduce synthetic server-tool terminal publication through the real streaming runtime.** Add focused regressions for router-emitted `max_tool_iterations` and `tool_errors` terminals that observe an intermediate native Responses id, expose the synthetic terminal, publish no continuation row, and reject the resulting public id before any follow-up upstream request.
- [x] **Step 12: Carry explicit clean provider completion provenance.** Preserve the final upstream `ResponseCompleted` terminal only for a genuine final Responses round, keep synthetic router terminals as `Finish`, and require typed streamed Responses completion evidence before continuation finalization. Preserve ordinary provider-origin `FinishReason::Other`, accumulated usage, framing, settlement, cancellation, and the existing final-round-only server-tool behavior.
- [x] **Step 13: Reproduce dynamic-auth authority replacement.** Exercise a registered `AuthApplier` whose stable continuation authority changes from credential/principal A to B under the same provider and account label. Prove unchanged A survives restart, changed B rejects the old reserved id before upstream, and a route/apply race cannot send the native id under B.
- [x] **Step 14: Bind the actual transport credential authority generically.** Add a redaction-safe typed `AuthApplier` continuation-authority proof, resolve and carry the exact request-local proof used by transport through execution/finalization, include it in the owner-keyed registry fingerprint, and fail closed whenever a mapped dynamic authority is missing or changes. OpenAI Codex supplies its stable ChatGPT account principal rather than its refreshable access token; static providers retain secret fingerprinting. Preserve selected-chain-only matching, service/model exclusion, exact-target rewrite, and no fallback.
- [x] **Step 15: Run focused continuation/server-tool/auth tests plus formatting, strict Clippy, schema/privacy checks, and full workspace nextest until GREEN.** Confirm the worktree contains no runtime identity/key artifacts.
- [x] **Step 16: Commit a new signed review-correction follow-up without amending, resetting, or pushing.**
- [x] **Step 17: Reproduce non-stream synthetic continuation publication through the real runtime.** Add non-stream HTTP regressions for `max_tool_iterations` and `tool_errors` using a real `HttpExecutor`, `ServerToolLoop`, continuation registry, and Responses wire mock. Assert the original response preserves its synthetic finish reason, usage, and intermediate response id for audit/framing, but creates no registry row and its public gateway handle rejects before any follow-up upstream call.
- [x] **Step 18: Add a positive non-stream multi-round control.** Exercise a genuine function-call round followed by a genuine final Responses result, then prove the registry binds only the final provider id and a subsequent request forwards exactly that final id.
- [x] **Step 19: Carry typed non-stream provider-terminal provenance.** Keep public `ServerToolLoop::run -> ExecutionResult` source-compatible; add a crate-private outcome used by `Pipeline` that marks `Done`/`HandBack` as provider-exposed and both router truncation branches as synthetic. Record the typed bit in `PipelineContext`, combine it with the actual selected Responses target and native response id, and require `native_response_completed` for streaming and non-streaming finalization without branching on `FinishReason` strings or clearing audit data.
- [x] **Step 20: Fail closed on invalid Responses terminal identity and status in both modes.** Prove direct ordinary non-stream Responses records genuine completed/incomplete provider provenance only with a non-empty opaque id (whitespace remains opaque, only empty is invalid); missing/empty id and failed/missing/unknown status settle Failed, never bind, never render completed, and their public handle rejects before upstream. Validate streaming terminal event type against body status/id, reject contradictory/missing/unknown/empty terminals and EOF without a valid terminal as protocol/upstream failure, preserve real `response.failed`/`error` framing and no post-open fallback, and classify typed custom failed/unknown `ResponseCompleted` as Failed with truthful framing. Cover direct and server-tool paths without inferring from `FinishReason` or breaking non-Responses adapters that legitimately omit it.
- [x] **Step 21: Preserve the exact successful fallback target.** Store the exact `RoutingTarget` instance that produced each successful non-stream result and stream attempt; make settlement and required finalization consume that typed target directly instead of reconstructing it from non-unique provider/model/account tuples. Add streaming and non-streaming fallback regressions with duplicate tuples but distinct endpoints/credentials: hop 1 fails, hop 2 succeeds, the mapping binds hop 2, and restart resume reaches only hop 2 with the final native id.
- [x] **Step 22: Authorize streaming continuation commit only while returning a delivered terminal.** Keep required finalization inline and cancellation-aware while the stream guard retains disconnect ownership; detach durable settlement separately. A deterministic barrier must prove dropping the consumer while terminal hooks/finalization are blocked produces `ClientDisconnected` settlement and no transient or durable mapping, while a normally returned terminal has an immediately resolvable mapping and still surfaces required-finalizer failures. Normal drop/cancel and graceful drain have tracked compensation; a hard process crash after the provisional encrypted DB insert remains an explicit at-least-once ambiguity and may preserve the provider-completed mapping after restart.
- [x] **Step 23: Remove native Responses ids from observability.** Audit every `response_id` log/export/event path. Streaming and non-streaming Responses spans must never contain intermediate or final native provider ids; use the canonical public `brc_…` gateway id only where the request identity is available. Preserve non-Responses `gen_ai.response.id`, encrypted request-local continuation binding, and add sentinel scans across span attributes/events/export payloads.
- [x] **Step 24: Bind continuation authority to the proven effective wire auth scheme.** Extend the typed route-time/apply-time authority proof to carry both credential principal and a redaction-safe effective scheme, atomically with the authenticated request. Static transports derive the scheme they actually install (including Responses/Chat Bearer regardless of a nominal target default); dynamic built-ins report their installed scheme; legacy unproven Responses auth fails closed. Registry fingerprints and route/apply race checks use the proven effective scheme, never `RoutingTarget::auth_scheme`. Regressions cover nominal XApiKey with actual Responses Bearer and same-principal dynamic scheme replacement rejecting before upstream while preserving non-Responses compatibility.
- **Focused verification evidence:** `cargo test -p bitrouter continuation::tests:: -- --nocapture` — 36 passed; `cargo test -p bitrouter-sdk --all-features language_model:: -- --nocapture` — 542 passed, 2 ignored; `cargo test -p bitrouter-observe --features otel-http -- --nocapture` — 38 passed; `cargo test -p bitrouter-providers --all-features -- --nocapture` — 193 passed; `cargo test -p bitrouter-cloud-sdk --all-features -- --nocapture` — 82 passed across unit and device-flow integration tests.
- [x] **Step 25: Run focused continuation/server-tool/context/fallback/disconnect/auth/OTel tests, formatting, strict Clippy, schema/privacy checks, and full workspace nextest until GREEN.** Confirm the worktree contains no runtime identity/key artifacts. Final gates: `cargo fmt --all -- --check` and strict all-target Clippy passed; `cargo nextest run --all-features --no-fail-fast` passed 2,418 tests with 11 skipped; `cargo run -p dist-helper -- check` reported the schema and 50-provider/49-model registry current; the runtime identity/key artifact scan was empty.
- [x] **Step 26: Commit a new signed review-correction follow-up without amending, resetting, or pushing.**
- [x] **Step 27: RED-test the real HTTP delivery boundary before changing delivery code.** Add assembled non-stream `execute_detached` cancellation coverage proving handler/body drop still completes upstream and billing settlement but compensates continuation publication. Add real StreamHook Drop/Replace regressions and public SSE multi-frame delivery tests proving only the final successful downstream frame authorizes publication in that frame's poll; dropping after earlier created/done frames must remain Missing with `ClientDisconnected`, while the first returned final frame must resolve immediately.
- [x] **Step 28: RED-test registry linearization and cancellation-safe two-phase compensation before changing registry code.** Reproduce resolve-precheck versus bind-insert TOCTOU on one identity, finalizer DB-await cancellation with late completion, conditional-delete failure/cancellation retry without same-process visibility, and sequential required-finalizer partial publication. Make same-identity resolve/bind/rollback share one linearization domain, retain row ownership until confirmed compensation, and prevent partial multi-finalizer publication by rejecting multiple required finalizers at build time until a compositional atomic API exists.
- [x] **Step 29: RED-test settlement privacy before changing public context types.** Add a malicious ordinary settlement recorder that captures every public field and sentinel-scans its formatted state, proving native Responses ids are unavailable. Remove raw `response_id` from `SettlementContext` and every constructor/test; retain native identity only inside continuation-specific `RequiredFinalizationContext`, while preserving public gateway OTel identity and native non-Responses observability.
- [x] **Step 30: RED-test exact wire-auth inference before changing authority proof.** Through real dispatch, prove only case-insensitive `Bearer <nonempty>` Authorization or a sole nonempty x-api-key/x-goog-api-key establishes authority. Basic, AWS4, empty Bearer, non-UTF8 Authorization, and simultaneous Bearer plus x-key must remain unproven and fail closed before Responses continuation upstream dispatch; retain positive Bearer and x-key controls.
- [x] **Step 31: RED-test the complete production SSE lifecycle before changing the decoder/encoder chain.** Use real `HttpExecutor` SSE to prove completed plus duplicate/trailing events fails with zero bind. Validate explicit `event:` against JSON `type`, require a nonempty unique `response.created` id when present, require terminal id equality with created id, and buffer apparent success until EOF so post-terminal violations are consumed. Preserve terminal-only compatibility, unknown-before-terminal forward compatibility, and typed `response.failed` behavior.
- **Sixth-round RED evidence:** focused `cargo test -p bitrouter-sdk --all-features <test> -- --nocapture` runs failed at the expected assertions for strict auth-header shape, duplicate-finalizer build rejection, explicit SSE event/body mismatch, empty/duplicate/mismatched created identity, and terminal buffering. Focused `cargo test -p bitrouter continuation::tests::<test> -- --nocapture` runs failed through real Axum/`execute_detached`, public multi-frame SSE body polling, post-hook downstream terminal mutation, fresh-registry provisional resolve, forced SQLite delete failure/retry, late finalizer DB completion, malicious settlement capture, real dynamic-auth dispatch, and real `HttpExecutor` post-terminal SSE: current behavior exposed Active mappings, Completed settlement, raw native id, invalid authority dispatch, or unconsumed trailing lifecycle data exactly as each RED declared. The normal final-SSE-frame control remained GREEN.
- [x] **Step 32: Run focused continuation/server/e2e/language-model/auth/OTel/provider/cloud tests, formatting, strict Clippy, full workspace nextest without fail-fast, dist-helper, artifact/privacy, and staged-diff audits until GREEN.** Record exact counts and confirm no runtime identity/key artifacts. Final gates: `cargo test -p bitrouter-sdk --all-features --lib` passed 770 tests with 2 ignored; `cargo test -p bitrouter --lib` passed 973; the OTel suite passed 38; providers passed 193; cloud SDK passed 82 across unit and device-flow integration tests; `cargo fmt --all -- --check`, strict all-target Clippy, and `git diff --check` passed; `cargo nextest run --all-features --no-fail-fast` passed 2,433 tests with 11 skipped; `cargo run -p dist-helper -- check` reported the schema and 50-provider/49-model registry current; privacy scans found the native id only in `RequiredFinalizationContext`, and the runtime identity/key artifact scan was empty.
- [x] **Step 33: Commit one new signed sixth-round review-correction follow-up without amending, resetting, or pushing.** Verify the signature, parent hash, and clean worktree before handoff.
- **Seventh-round RED/GREEN contract:** Every production change below starts with a focused regression that fails against `d2a0bd963b82eb91fd5b549c3569a15002129657` for the named externally observable behavior. RED evidence must distinguish the intended assertion from setup/compiler failures. GREEN requires that same focused test plus its neighboring continuation/protocol/observability suite before moving to the next item; final completion additionally requires the broad gates in Step 37.
- [x] **Step 34: RED-test and repair durable continuation publication ownership and integrity.** Reproduce first-error active delete/demote followed by outer rollback/retry, direct `bind` racing a provisional owner, and two independent `ContinuationRegistry` instances sharing one database. Replace process-only attempt ownership with a durable random generation token, enforce legal publication states, and authenticate state plus token in AEAD AAD. Every activate/demote/delete must use owner-aware compare-and-swap predicates including old state, token, ciphertext, and nonce; state transitions that change AAD must atomically re-encrypt and conditionally update the complete row. A second attempt for the same identity must serialize or fail closed without sharing ownership, while different identities remain concurrent and an already-active identical binding remains idempotent. Ambiguous or failed compensation retains retryable ownership and same-process visibility remains `Missing`. Adjust the unpublished Task 11 migration sequence/schema as needed and prove final migrator/backfill plus SQLite/Postgres/MySQL SQL generation. GREEN evidence: 53 focused continuation tests passed; four final-schema/constraint/migrator/portable-SQL tests passed; migration 013 now requires authenticated state/generation values on SQLite, Postgres, and MySQL SQL generation and unpublished migration 014 is absent from the final migrator.
- [x] **Step 35: RED-test and repair post-terminal SSE validation and native-id privacy.** Through real `HttpExecutor`, prove `response.completed` followed by `[DONE]` or malformed nonempty data fails before publication; reject every nonempty post-terminal datum before JSON parsing while preserving terminal-only compatibility, unknown-before-terminal forward compatibility, and typed `response.failed`. Make created/terminal mismatch and all lifecycle diagnostics value-free. Add an ordinary malicious settlement recorder, tracing/log capture, and failed-stream OTel export sentinel scan proving neither mismatched native id appears in public context, logs, events, exception text, or exported attributes. GREEN evidence: `cargo test -p bitrouter-sdk --features config_file --lib responses_stream_ -- --nocapture` passed all 16 lifecycle/compatibility tests; the real-HTTP `[DONE]`/malformed/duplicate/unknown post-terminal matrix passed 1 test with four cases and zero publication; the real-HTTP mismatch privacy test passed while scanning caller errors, an ordinary malicious `SettlementRecorder`, and a real `tracing_subscriber::fmt` writer sink; `cargo test -p bitrouter-observe --features otel-http --lib otel::exporter::hop_tests::responses_mismatch_native_ids_never_enter_failed_stream_spans -- --nocapture` passed its full failed-hop/root exported `SpanData` sentinel scan.
- [x] **Step 36: RED-test and repair outbound trace mutation and final wire-auth proof.** Add a malicious `ObserveHook` real-dispatch regression that attempts to overwrite Authorization and x-api-key after authority proof. Restrict `set_outbound_trace_headers` to exact W3C `traceparent` and `tracestate`, reject or ignore every other header without mutating the request, and re-prove/revalidate the final wire auth scheme and authority after all request mutations and immediately before dispatch. Preserve positive W3C propagation and sole Bearer/x-api-key/x-goog-api-key controls. Evaluate a source-compatible shim for the public `StreamPart::ResponseStarted`/`ResponseCompleted` provenance field; if typed provenance makes compatibility impossible in this round, record the constructor/pattern/serde migration risk and rationale explicitly in this plan. GREEN evidence: the real `ObserveHook` resume-dispatch regression passed while attempting `Authorization`, `x-api-key`, `x-goog-api-key`, and request-id replacement and simultaneously proving positive `traceparent`/`tracestate` propagation plus the original native parent; the existing malformed/ambiguous and sole Bearer/x-key real-dispatch matrix passed, as did the OTel W3C injection control and SDK auth unit suite. Final authority validation now occurs after auth, trace merge, and request-id injection, immediately before the built request is returned for dispatch.
- **Step 36 compatibility assessment:** this is an additive typed-provenance breaking migration; no Rust source-compatible shim can add a required field to an existing public enum struct variant. Making `source_protocol` optional/defaulted, adding serde defaults, or offering associated constructors cannot make legacy `StreamPart::ResponseStarted { id }` / `ResponseCompleted { id, status, usage }` struct literals compile and cannot preserve exhaustive field patterns. Restoring the old shapes or treating absent provenance as a usable default would erase the typed source boundary that prevents a Chat/Messages/Generate Content id from becoming a Responses continuation; a new parallel variant would still be a breaking exhaustive-enum change and would make legacy hooks miss the typed lifecycle. The retained migration risk is therefore explicit: downstream constructors and serialized producers must provide `source_protocol`; downstream patterns should bind it or use `..`; serialized consumers that reject unknown fields must be updated for it. This is justified by the fail-closed continuation security boundary and should be called out in the next SDK/API release notes.
- [x] **Step 37: Run focused registry/migration/continuation/server/e2e/protocol/privacy/OTel/auth tests and all broad gates until GREEN.** Run SDK, application, Observe, provider, and cloud suites; `cargo fmt --all -- --check`; strict all-feature/all-target Clippy; full workspace nextest without fail-fast; dist-helper; schema/privacy/runtime-artifact/staged-diff audits. Record exact counts and confirm no runtime identity/key artifacts. Final evidence: continuation passed 55 tests; migration passed 3 continuation-schema tests plus the final-migrator test; SDK passed 770 with 2 ignored; application passed 984; Observe/OTel passed 39; providers passed 193; cloud SDK passed 82 across 77 unit and 5 device-flow tests. `cargo fmt --all -- --check`, `git diff --check`, and strict all-feature/all-target Clippy passed. `cargo nextest run --all-features --no-fail-fast` passed all 2,445 tests with 11 skipped. `cargo run -p dist-helper -- check` reported the schema plus 50-provider/49-model registry current. Privacy checks found the only public `response_id` settlement field inside continuation-specific `RequiredFinalizationContext`, lifecycle diagnostics remain value-free, and the runtime identity/key artifact scan was empty.
- [x] **Step 38: Commit one new signed seventh-round review-correction follow-up without amending, resetting, or pushing.** Verify the signature, exact parent `d2a0bd963b82eb91fd5b549c3569a15002129657`, and clean worktree before handoff.
- **Seventh specification-review verdict:** FAIL, 0 Critical / 3 Important / 1 Minor. An independent two-registry SQLite probe resolved an authenticated Active mapping after activation-ready but before delivery acknowledgment, because the durable row becomes Active before `wait_for_delivery` and only the activating registry's process-local pending map hides it. An ambiguous insert that committed before returning a driver error is reread as an undifferentiated foreign provisional row, causing the caller to clear the same generation's retry ownership. Expiry scrub and purge read stale rows but mutate by identity only, so another registry can replace the row with a new generation that the stale scrub clears or the stale purge deletes. The provenance-field compatibility break remains Minor and is adequately disclosed for this phase, with SDK/API release-note follow-through required. The reviewer independently reconfirmed all sixth-round SSE, privacy, authenticated-state, trace-header, and final-wire-auth corrections and reran continuation 55, Responses lifecycle 16, migration 3+1, App 984, SDK 770 with 2 ignored, Observe 39, formatting, strict Clippy, and diff checks successfully; clean gates do not override the three concurrent-state blockers.
- **Eighth-round RED/GREEN contract:** Every production change below starts with a deterministic focused regression that fails against `832822948bc9bf39312298c210247cb2e846f282` at the intended assertion. The repair remains generic and owner/identity/generation based: it must not inspect or inject benchmark, task, prompt, model, workflow, case, or private caller metadata. GREEN requires each focused regression plus the neighboring continuation/migration/delivery suite; Task 11 still requires a fresh specification PASS and then a separate quality PASS before any push.
- **Eighth-round RED evidence:** the two-registry ready-before-ack regression failed because the second registry resolved `Active`, and the SDK permit regression failed because the permit returned immediately after acknowledgment instead of awaiting finalizer activation. The committed-insert fault failed by leaving the matching generation orphaned. Deterministic resolve scrub, batch scrub, and purge pauses failed by redacting or deleting a rebound foreign generation. Each was an assertion failure against `832822948bc9bf39312298c210247cb2e846f282`, not a setup or compiler failure.
- [x] **Step 39: RED-test and repair cross-registry delivery visibility.** Use two independent `ContinuationRegistry` instances sharing a real SQLite database and key. Pause after the finalizer announces readiness but before downstream acknowledgment and prove the second registry resolves `Missing`; then cover negative acknowledgment/early drop with no transient or durable Active mapping and a normal delivered control with immediate successful follow-up. Introduce an authenticated durable pre-delivery state distinct from resolvable Active, and make the delivery protocol perform the owner-generation CAS to Active only after the downstream acknowledgment has been accepted. Preserve cancellation-safe compensation, exact same-binding idempotency, restart behavior, and the explicitly documented hard-process-crash ambiguity; process-local locks or pending maps may optimize but cannot define correctness. GREEN uses authenticated durable `delivering`, a post-ack owner/generation CAS, and a drop-aware activation-complete/terminal-commit rendezvous; independent registries never resolve pre-ack or early-drop rows, while the returned terminal is immediately resolvable.
- [x] **Step 40: RED-test and repair ambiguous-insert ownership recovery.** Inject an `INSERT committed, driver returned error` result, reread the committed row, and prove a matching publication generation is recognized as this attempt's ambiguous success without clearing ownership. A different generation remains foreign and fail-closed. Exercise rollback/retry and restart cleanup so no orphan provisional row permanently blocks the identity. The matching generation now retains rollback ownership after an ambiguous commit and is removable after restart; a paused delete/rebind control proves a foreign generation is never adopted.
- [x] **Step 41: RED-test and repair stale prune/scrub races.** Deterministically pause after reading an expired or purgeable row, replace it through a second registry with a new generation, then resume the stale maintenance operation. Require scrub and delete to predicate on the complete authenticated row snapshot, including identity, state, generation, ciphertext, nonce, and relevant expiry/purge boundaries; a zero-row CAS is a stale no-op/reload and must never clear or delete the replacement. Cover both opportunistic resolve-time scrub and batch prune/purge. Resolve scrub now reloads after a zero-row CAS; batch scrub and purge predicate on the full selected row, so all three rebound-generation regressions preserve the replacement.
- [x] **Step 42: Run focused registry/migration/delivery/restart tests and the full Task 11 gate set until GREEN.** Rerun SDK, application, Observe, provider, and cloud suites; formatting, diff, strict all-feature/all-target Clippy, full workspace nextest, dist-helper, privacy/runtime-artifact/schema audits. Record exact counts and preserve a clean worktree. Final evidence: continuation passed 66 tests; migration passed 3 continuation-registry tests; SDK passed 772 with 2 ignored; application passed 995; Observe/OTel passed 39; providers passed 193; cloud SDK passed 82 across 77 unit and 5 device-flow tests. `cargo fmt --all -- --check`, `git diff --check`, and strict all-feature/all-target Clippy passed. Fresh `cargo nextest run --all-features --no-fail-fast` passed all 2,458 tests with 11 skipped. `cargo run -p dist-helper -- check` reported the schema plus 50-provider/49-model registry current. Privacy and migration audits found no caller metadata dependency, no new migration 014, and no native Responses id in ordinary settlement context; the runtime identity/key/database artifact scan was empty.
- [ ] **Step 43: Commit one signed eighth-round correction without amending or resetting, build a fresh full seven-plus-one-commit review package, and repeat independent specification then quality review before push.** This remains intentionally open until the signed follow-up is verified and the fresh independent specification and quality reviews complete; no push occurs before both PASS.
- **Eighth independent review verdicts:** specification PASS, 0 Critical / 0 Important / 1 disclosed compatibility Minor; fresh quality FAIL, 0 Critical / 1 Important / 1 Minor. The quality reviewer found a remotely repeatable unbounded process-memory path: a caller reuses an already-successful `x-bitrouter-request-id` for new root Responses requests, each provider completion produces a different native id/generation, `bind_inner` conclusively rereads a foreign/different Active binding, but `bind_pending` treats every error as insert ambiguity and retains its `pending_publications` entry. Tracked rollback then sees the foreign generation, returns before clearing, and the map has no TTL/capacity/background reap. Repeating the request grows one permanent entry per failure while the durable row remains unchanged. The `source_protocol` source/serde break remains the disclosed Minor. Independent gates still passed continuation 66, SDK 772 with 2 ignored, delivery 2, post-terminal 1, migration 3+1, formatting, and diff checks; clean tests do not override the remotely triggerable ownership leak.
- **Ninth-round RED/GREEN contract:** The correction must remain generic and keyed only by request attempt, owner-bound continuation identity, durable generation, and binding evidence. It must not inspect or inject task, benchmark, prompt, workflow, case, model, or private caller metadata. Every production change begins with an assertion-level RED against `47003b3743dcad8f3b3240896f2a8827e132e04e`; specification and fresh quality must both PASS again before any Task 11 push.
- **Ninth-round RED evidence:** against exact parent `47003b3743dcad8f3b3240896f2a8827e132e04e`, the assembled real-HTTP repeated-root regression returned the expected fail-closed 500 but failed its first bounded-state assertion with `pending_publications` 1 rather than 0. The foreign provisional and delivering controls failed because tracked rollback still reported generation mismatch, and the reliable rejected-INSERT/absent-reread control failed with one impossible marker retained. These were assertion failures, not setup/compiler failures. The same-generation committed ambiguity and real-SQLite database-unreadable controls remained GREEN, proving the required retain side independently.
- [x] **Step 44: RED-test deterministic ownership loss and bounded local state.** Through the assembled Responses HTTP path, reuse one caller-chosen request id after an initial successful root while the provider returns a different native id on each new root. Prove every collision fails closed, preserves the original Active durable row, and leaves `pending_publications` at its baseline after tracked rollback rather than growing once per request. Add focused controls for a foreign provisional/delivering generation, a different binding, a conclusively absent row, same-generation committed ambiguity, and a database read/compensation error whose ownership remains genuinely unknown. GREEN repeats the root three times with distinct native ids, preserves the byte-for-byte durable Active model and original resolved provider id after each 500, and observes a zero marker count every time. Foreign provisional/delivering and absent-row controls clear only the contender marker; table-rename faults retain Unknown through bind and compensation until a later owner-aware rollback can decide.
- [x] **Step 45: Implement typed bind and compensation ownership outcomes.** Distinguish `Owned`, `Lost/Foreign`, and `Unknown` instead of retaining local ownership for every `anyhow::Error`. When the durable reread proves a different generation/different binding or no owned row, clear only the local attempt marker and never transition/delete/scrub the foreign row. Same-generation ambiguous success retains retry/rollback ownership; database ambiguity retains ownership until a later owner-aware rollback can decide. Make rollback clear conclusively lost ownership even when it reports the original collision, while same-generation CAS/DB errors remain retryable. Preserve all Step 39–41 delivery, restart, and maintenance semantics. `PublicationOwnership` now travels with typed bind/compensation failures; compensation separately reports released or conclusively lost state. Local removal still requires the exact attempt id plus generation, the current request retains its original collision/error, and no foreign durable row is mutated.
- [x] **Step 46: Rerun focused collision/ownership/delivery/registry/migration tests and every full Task 11 gate.** Require SDK, App, Observe, providers, cloud, formatting, diff, strict all-feature/all-target Clippy, full nextest, dist-helper, privacy/task-independence/migration/runtime-artifact audits, exact counts, and a clean worktree. Final evidence: continuation passed 71 tests; migration passed 3 continuation-registry tests plus the final-migrator test; SDK passed 772 with 2 ignored; application passed 1,000; Observe/OTel passed 39; providers passed 193; cloud SDK passed 82 across 77 unit and 5 device-flow tests. `cargo fmt --all -- --check`, `git diff --check`, and strict all-feature/all-target Clippy passed. Fresh `cargo nextest run --all-features --no-fail-fast` passed all 2,463 tests with 11 skipped. `cargo run -p dist-helper -- check` reported the schema plus 50-provider/49-model registry current. Parent-diff privacy/task-independence/migration audits found only generic owner/identity/generation evidence, no caller task/benchmark/prompt/model/workflow/case metadata, no SDK settlement or migration change, and no migration 014; the runtime identity/key/database artifact scan was empty.
- [ ] **Step 47: Create one signed ninth-round follow-up without rewriting history, build a fresh nine-commit package, and require independent specification PASS followed by fresh quality PASS before push.** This remains intentionally open until the signed follow-up is verified and fresh independent specification then quality reviews both PASS; no push occurs first.
- **Ninth independent specification verdict:** FAIL, 0 Critical / 2 Important / 1 Minor. First, duplicate `delivery_attempt_id` rejection occurs before the duplicate obtains ownership, but the SDK automatically invokes rollback for every finalize error; rollback addresses only the shared attempt id and therefore compensates the first valid publication. A dynamic production-equivalent probe observed the original marker fall from 1 to 0 after duplicate finalize error plus automatic duplicate-context rollback. Second, Unknown bind/compensation ownership has no production retry, sweep, TTL, or bound: an assembled HTTP probe performed three committed inserts whose reread and sole automatic rollback were temporarily unreadable, restored the DB between requests, and observed permanent marker growth 1→2→3 even after an additional drain. The retained-marker Active visibility risk is adjacent: resolve accepts durable Active without a reconciliation gate; the reviewer identified this statically but did not count it separately. The public `source_protocol` migration remains Minor. Fresh continuation 71, SDK 772 with 2 ignored, Observe 39, formatting, and diff checks passed; clean gates do not close the two lifecycle findings.
- **Tenth-round RED/GREEN contract:** Correctness must not depend on globally unique attempt ids never colliding or on a caller/test manually invoking rollback after database recovery. The solution remains generic and owner/identity/generation based, with no task/benchmark/prompt/model/workflow/case/private-metadata channel. Every production change starts with an assertion-level RED against `5191c91084a65ee99974426e982172440b14568e`; both independent reviews must repeat before push.
- **Approved tenth-round design:** The SDK will add an opaque, invocation-specific receipt through additive defaulted finalizer methods while retaining the legacy trait surface; every production finalize, commit, disconnect, drop, and error rollback path must carry the same receipt, and Continuation may mutate only the marker authenticated by that receipt. Unknown ownership will be handled by one registry-scoped bounded/deduplicated worker with a pre-side-effect capacity reservation, a worker-invisible Reserved phase, fixed batches, bounded backoff, no per-request tasks, no eviction, and fail-closed backpressure. The unpublished migration 013 may add generic CSPRNG process-instance and lease-expiry evidence plus an indexed state/lease scan. Lease renewal, state transitions, compensation, deletion, and orphan claiming must be fenced by generation, instance, prior lease, state, and the relevant immutable row snapshot; instance/lease changes must be authenticated with the encrypted row. Production assembly starts the worker and bind lazily starts it as a fallback; graceful drain stops it in bounded time without deleting unresolved ownership. Restart reconciliation may claim only expired provisional/delivering rows and must fence a still-live instance; it never sweeps Active. In the ordinary process-alive path, a retained exact identity/generation marker hides an Active row until reconciliation converges. The already accepted narrow at-least-once exception remains: a hard process crash after durable Active transition but before socket return can leave Active visible after restart because no durable fact distinguishes terminal return.
- [x] **Step 48: RED-test and add attempt-specific finalizer preparation/rollback attribution.** Reproduce the production SDK path: finalize A successfully, finalize B with the same delivery attempt id and a different identity/binding, then let the pipeline's automatic finalize-error rollback run for B. Require A's marker and durable provisional row to remain intact and independently rollback/commit correctly. Cover same-identity/different-binding and exact-duplicate contexts plus opposite completion order. Add an additive/backward-compatible required-finalizer preparation outcome/receipt (or equivalent unforgeable attribution) so rollback runs only for the invocation that actually obtained ownership; a duplicate rejected before receipt acquisition must never address the existing attempt's generation. Production controls now prove B's automatic rollback leaves A intact, both completion orders preserve A, and aborting A followed by the real pipeline drain lets only A's receipt remove its marker and exact durable row.
- [x] **Step 49: RED-test and implement bounded production reconciliation for Unknown ownership.** Through assembled HTTP, reproduce at least three committed-insert+reread-Unknown and automatic-compensation-Unknown attempts, restore the database without manually calling `ContinuationRuntime::rollback`, and require production reconciliation to converge every owner/generation marker and durable row while memory/tasks remain bounded and deduplicated. Exercise persistent outage, shutdown/restart, same-generation Owned retry, conclusively Lost/Foreign cleanup, and an Active-transition/terminal-drop compensation fault; resolve must not expose an Active mapping whose terminal was not committed while reconciliation is pending. Reconciliation must be driven by durable owner/generation evidence with bounded scheduling/backoff and lifecycle integration, not a test-only retry, unbounded per-request task, TTL-only deletion, or process-local cap that discards unresolved ownership. All production-path, capacity, fairness, outage, owner/fencing, restart, Active-mask, shutdown, AAD, and migration controls now pass without caller/test rollback or per-request reconciliation tasks.
- **Tenth-round primary RED evidence:** On exact candidate `5191c910`, a real Pipeline test paused A after successful preparation, forced B to share A's delivery-attempt id, and let the SDK execute its normal finalize-error rollback. The different-identity, same-identity/different-binding, and exact-duplicate matrix failed at the first ownership assertion with marker count `0` instead of `1` (`automatic duplicate-context rollback removed the first invocation marker`); a late-B-rollback/A-first-commit ordering control is included. A separate assembled Responses HTTP test repeated committed-insert followed by unreadable bind-reread and sole automatic compensation three times, restored the database after each request, never called runtime rollback, and performed the normal drain. It failed at retained counts `[1, 2, 3]` instead of `[0, 0, 0]` (`automatic production reconciliation retained Unknown markers after recovery`). Both failures are production-path assertion failures rather than setup, compilation, or loopback failures; GREEN may now change production code.
- **Tenth-round receipt GREEN so far:** The SDK now creates an opaque `Arc`-identity receipt per required-finalizer invocation and carries it through receipt-aware defaulted prepare, commit, disconnect, error, drop, streaming, and non-streaming paths; legacy trait methods remain available for source compatibility. Continuation markers compare the exact receipt before any rollback/activation mutation, and the collision test wrapper explicitly forwards the real SDK receipt while changing only the test attempt id. Focused receipt identity, B-first rollback matrix, and A-first-commit/B-late-rollback controls pass 3/3. Step 48 remains open until the original A-receipt rollback control and complete focused/full regressions pass.
- **Tenth-round fencing/worker GREEN so far:** The bounded registry worker is a single weakly-owned task with fixed capacity 256, batch size 32, deduplication by identity/generation, Notify wakeup, and bounded backoff; production assembly starts it and bind starts it lazily, while the additive finalizer drain hook cancels/awaits it with bounded shutdown. Reserved entries are invisible to reconciliation and capacity exhaustion fails before prune/insert. Migration 013 adds CSPRNG instance ownership, lease expiry, and a state/lease index. All lease renewal, state transition, compensation, deletion, and stale provisional/delivering claim operations use full-snapshot CAS; generation, instance, exact lease, state, ciphertext, nonce, and immutable row fields fence ownership, while instance/lease are included in the 11-field AEAD AAD and renewal/claim re-encrypt atomically. Activation no longer holds the identity lock across acknowledgement or terminal waits; every DB segment reloads the current marker and renews a near-expiry lease before mutation. The worker holds only `Weak<Inner>` and the inner type remains private. `cargo check -p bitrouter --all-features --tests`, the three core receipt tests, and 55 non-network continuation tests pass; 19 WireMock cases reached sandbox `PermissionDenied` on local bind and require the normal escalated rerun. Step 49 remains open for the complete capacity/outage/restart/Active-mask/shutdown matrix and full gates.
- **Mid-GREEN worker audit:** A fixed `HashMap.iter().take(batch)` snapshot is not fair: with more than 32 pending publications, the same entries can be renewed every pass while later live Prepared entries expire, and the same registry's stale sweep can then claim and compensate its own still-delivering work. Before Step 49 can close, scheduling must use a bounded round-robin/cursor that services the entire 256-entry capacity within the renewal window, stale orphan scanning must skip every exact local Reserved/Prepared/Reconcile owner marker, and failed/poisoned stale rows must not permanently starve later rows. A dynamic control must keep more than one batch of live Prepared publications fenced across multiple passes and then allow each original receipt to commit or rollback normally.
- **Tenth-round focused GREEN after audit:** Stable attempt-id ordering plus a persisted last-key cursor now provides bounded round-robin renewal; a 48-entry test deletes four already-scanned entries, inserts four low-id replacements, and still renews/preserves all 48 original/live owners without self-claim. Graceful shutdown cancels the sole worker and loops local-only batches under one two-second deadline, reporting failure without discarding evidence if it cannot progress. Dynamic restart evidence covers unexpired live leases, expired provisional/delivering takeover, Active exclusion, and an old stale snapshot losing full-snapshot CAS after the originating owner renews and re-authenticates its lease. The Active terminal-drop test transitions durably Active, hides the table so compensation ownership is genuinely Unknown, restores the DB with the worker suspended, observes same-instance Missing versus fresh-instance Active, proves restart sweep excludes Active, and then converges automatically when the owner worker resumes. Lost/foreign generation cleanup preserves the replacement row. Focused Task 11 matrix passes 16/16, the receipt primitive passes 1/1, and the full continuation suite passes 83/83 with the required local-bind permission. Step 49 remains open only until migration/AAD and broader regression gates confirm the integrated tree.
- **Tenth-round integrated GREEN:** Migration 013 structure plus final migrator/AAD/renew/race integration passes 4/4, and the migration-focused suite passes 5/5. SDK passes 773/773 with 2 skipped; the application passes 1,182/1,182 with 9 skipped; continuation passes 83/83; Observe/OTel passes 39/39; providers pass 193/193; cloud passes 82/82. Fresh all-feature workspace nextest passes 2,476/2,476 with 11 skipped. One initial unrelated trajectory concurrency flake passed its isolated rerun 1/1 and the complete fresh workspace rerun. `cargo fmt --all -- --check`, strict all-feature/all-target Clippy with warnings denied, and `git diff --check` pass. Step 50 remains open for dist-helper plus final task-independence/privacy/no-014/runtime-artifact audits and clean-tree evidence.
- [x] **Step 50: Rerun duplicate attribution, reconciliation, delivery, registry, migration, restart and every full Task 11 gate.** Record exact counts for SDK/App/Observe/providers/cloud/full nextest; require formatting, diff, strict Clippy, dist-helper, task-independence/privacy/migration/runtime-artifact audits and a clean worktree. Final evidence: continuation 83/83; SDK 773/773 with 2 skipped; application 1,182/1,182 with 9 skipped; Observe/OTel 39/39; providers 193/193; cloud SDK 82/82; fresh all-feature workspace nextest 2,476/2,476 with 11 skipped. Migration 013/final-migrator/AAD/renew/stale-claim integration passed 4/4 and migration-focused tests passed 5/5. Formatting, diff check, strict all-feature/all-target Clippy, and dist-helper passed. The final production metadata scan found no task/benchmark/prompt/model/workflow/case channel; leakage scan found no native provider id, key, owner, generation, instance, or lease value added to logs/errors; only unpublished migration 013 and its tests changed with no 014; runtime artifact scan found no generated database/key/log artifacts. Seven tracked files are modified with no untracked files before the plan update and signed commit.
- [ ] **Step 51: Create one signed tenth-round follow-up without rewriting history, build a fresh ten-commit package, and require independent specification PASS followed by fresh quality PASS before any push.**
- **Tenth independent specification verdict:** FAIL, 0 Critical / 1 Important / 1 Minor. The prior duplicate-receipt and bounded-reconciliation Importants are closed, and the suspected poison-row scheduling gap is disproved because local pending work forces a one-second cadence: 256/32 completes within eight seconds, below the 15-second renewal window and 30-second lease. The new Important is graceful shutdown: `ContinuationRuntime::drain_pending_work` returns an error and retains a known same-instance Active reconciliation marker during a database outage, but `Pipeline::drain_pending_settlements` only logs the error and reports successful completion. Normal process exit then loses the only Active mask; restart intentionally excludes Active from stale lease takeover and resolves the uncommitted terminal as Active. An exact-HEAD production-state probe observed `Active(provider-shutdown-active)` instead of Missing after the failed graceful drain and restart. This is not the accepted hard-crash exception because the shutdown path observed the unresolved ownership and still completed normally. The disclosed `source_protocol` public/serde migration remains Minor.
- **Eleventh-round boundary:** Do not add a new durable terminal-commit gate solely to erase the already accepted hard-crash ambiguity. Instead, make graceful shutdown fail closed at the process lifecycle: once the HTTP server has stopped accepting new connections and drained in-flight handlers, unresolved required-finalizer ownership prevents the server future from completing successfully. Each reconciliation attempt remains bounded and preserves evidence; the production shutdown loop retries with bounded backoff until the database recovers and the same-instance marker/Active row are compensated. An external force-kill during that wait is explicitly a hard crash and remains inside the documented exception. Keep the existing source-compatible best-effort drain API for callers/tests, and add an error-returning required drain used by production shutdown.
- [x] **Step 52: RED-test propagation of a failed required-finalizer drain.** Build the exact state from the reviewer probe: durable post-ack Active, database hidden before terminal-drop compensation, ownership Unknown, and one retained exact marker. The new required pipeline drain must return an error without clearing the marker or reporting graceful completion. Restore the database and retry through the same production drain API without any manual runtime rollback; require marker and durable row to reach zero, then drop/restart and resolve Missing. GREEN reproduces the reviewer state against real SQLite: the first required drain returns `Err` with one retained marker, recovery plus the same API removes the exact marker and row without manual rollback, and a restarted registry resolves Missing.
- [x] **Step 53: RED-test and implement the production graceful-shutdown retry loop through every real entry point.** After axum has stopped accepting traffic and completed in-flight handlers, call only the error-returning required drain. A first reconciliation failure must keep the server shutdown future pending; use bounded per-attempt work and a fixed bounded retry delay without spawning per-error tasks or accepting new requests. Database recovery must let the same future finish successfully. Preserve `drain_pending_settlements() -> usize` as an additive compatibility wrapper for existing external/test callers, but production server code must not use the swallowing path. Add an external-shutdown variant of the SDK App serving API while preserving existing standalone signal-driven methods. The `apps/bitrouter` daemon must signal that external shutdown on term/control completion and continue awaiting the same HTTP future through required-drain recovery; its outer `select!` must never drop the HTTP future and bypass the gate. HUP listener failure remains non-terminal and must only log while serving continues. GREEN adds the error-returning Pipeline API, keeps the legacy best-effort method unchanged, waits for the same axum future before serial 250ms required-drain retries, and exposes additive external-shutdown serve variants. The daemon now signals shutdown on term/control and continues polling that same HTTP future; HUP setup failure only disables HUP and direct HTTP errors still propagate.
- **Eleventh-round RED evidence:** On exact `2e2f3e95`, the reviewer-state Pipeline test fails compilation at two calls because the required error-returning drain API does not exist. SDK server lifecycle tests fail at two calls because `complete_graceful_shutdown` does not exist; they model axum completion before draining, recovery after the first error, and 64 paused-time persistent failures with exactly one active serial drain future. The real daemon orchestration test fails at two calls because `supervise_http_shutdown` does not exist; it covers both term/control triggers, HUP setup failure remaining non-terminal, accept disabled before in-flight release, the same HTTP/supervisor future remaining pending after the first drain failure, recovery completion, and direct HTTP-error propagation. All three are intended compile-time REDs on the missing production interfaces, not setup or loopback failures.
- [x] **Step 54: Rerun the reviewer shutdown/restart probe, Task 11 focused/full gates, and every static audit.** Reviewer SQLite probe passed 1/1; shutdown-focused tests passed 4/4; outer daemon orchestration passed 2/2; continuation passed 84/84; SDK passed 777/777 with 2 skipped; application passed 1,185/1,185 with 9 skipped; fresh all-feature workspace nextest passed 2,483/2,483 with 11 skipped. The first workspace run exposed two pre-existing trajectory timestamp concurrency flakes; both isolated controls passed 2/2 and the complete fresh rerun passed. Formatting, `git diff --check`, strict workspace all-feature/all-target Clippy, dist-helper (50 providers / 49 models), task-independence/privacy/no-014/runtime-artifact audits all pass. The pre-commit tree contains only the five intended implementation/test files plus this plan and no untracked runtime artifacts.
- [ ] **Step 55: Create one signed eleventh-round follow-up without rewriting history, build a fresh eleven-commit package, and require independent specification PASS followed by fresh quality PASS before any push.**
- **Eleventh independent specification verdict:** FAIL, 0 Critical / 1 Important / 1 Minor. The original graceful-shutdown Important is closed by an independent real-SQLite Active→Unknown probe, the public external-shutdown App path, and focused SDK/Pipeline/daemon tests. The new Important is the adjacent daemon restart lifecycle: the Stop handler acknowledges, `run_control_socket` cleans up the control endpoint, and only then the supervisor signals and awaits the HTTP/required drain. Existing `restart()` waits only for that already-released endpoint, immediately calls `start()`, and fails with `already running` while the old PID is still legitimately draining. A long in-flight request makes this a race; a required-drain outage makes it deterministic. It remains fail-closed and does not expose Active, but it is a production compatibility regression. The disclosed `source_protocol` migration remains Minor.
- **Twelfth-round boundary:** Preserve the new fail-closed graceful-reconciliation gate and the existing 30-second restart grace period/force-kill policy. Restart must capture the old daemon PID before Stop, wait for that exact process to exit rather than treating control-endpoint release as process completion, and only force-kill after the grace period. Missing/stale PID evidence must remain conservative and retain a safe endpoint fallback. The correction stays generic process-lifecycle logic and may not inspect request, continuation, provider, model, task, benchmark, prompt, workflow, case, or private caller data.
- [x] **Step 56: RED-test restart while the control endpoint releases before the old process exits.** Model Stop acknowledgement and immediate endpoint cleanup while the recorded old PID remains alive through an in-flight/required-drain wait. Require restart supervision to remain pending rather than enter `start()`/report `already running`; process exit inside the grace period must allow restart. Add a timeout control proving force-kill is not requested early and is requested only after the full grace period. Permanent paused-time coverage now proves endpoint release alone cannot advance restart, natural PID exit still waits endpoint cleanup, force-kill occurs only at the full grace boundary, the cleanup window remains bounded, missing PID evidence never triggers a guessed kill, and stale PID-file removal requires an exact dead match. A structural wiring control fixes PID capture before Stop and keeps the release gate before replacement start, including already-draining invocations whose endpoint is already absent.
- **Twelfth-round RED evidence:** On exact `73ef1bfe`, `restart_does_not_treat_endpoint_release_as_process_exit` failed 0/1 with exit 101 at the intended production decision: `wait_for_socket_release(..., ZERO)` returned ready solely because the endpoint was absent even though the exact recorded old PID remained alive. The assertion reported `restart advanced after endpoint release while exact old pid ... was alive`. This is a behavioral RED, not a compiler/setup/loopback failure.
- [x] **Step 57: Implement exact old-process exit waiting with conservative fallback.** Capture the recorded PID before sending Stop, poll that PID until it exits, then ensure the endpoint is released before starting the replacement. If the exact process survives the 30-second grace period, invoke the existing force-kill path, briefly await exit/endpoint cleanup, and remove only its stale PID file. When no valid PID is available, retain the bounded endpoint-release fallback rather than guessing another process identity. Keep Stop response and the fail-closed HTTP/required-drain loop unchanged. GREEN uses a private restart release gate with the existing 30-second grace, 50ms polling, and two-second cleanup interval. A captured PID is always awaited even after the endpoint disappears; timeout kills only that PID and requires PID-dead plus endpoint-free before replacement. Missing PID evidence never kills, and conditional PID-file cleanup cannot erase a replacement process's evidence.
- [x] **Step 58: Rerun restart lifecycle, graceful-shutdown/reviewer probes, Task 11 full gates, and static audits.** Restart-focused tests passed 7/7; actual-wiring control passed 1/1; outer daemon supervision passed 2/2; reviewer SQLite required drain passed 1/1; SDK shutdown passed 4/4; continuation passed 84/84; application passed 1,192/1,192 with 9 skipped; SDK passed 777/777 with 2 skipped. Fresh all-feature workspace nextest passed 2,490/2,490 with 11 skipped. The first workspace run hit the already recorded trajectory timestamp concurrency flake; its exact isolated control passed 1/1 before the complete fresh rerun. Formatting, `git diff --check`, strict workspace all-feature/all-target Clippy, dist-helper (50 providers / 49 models), task-independence/privacy/no-014/runtime-artifact audits all pass. The only sandbox-only failure was the SDK external-shutdown localhost bind denial; the exact authorized rerun passed 1/1. The pre-commit tree contains only `apps/bitrouter/Cargo.toml`, `apps/bitrouter/src/main.rs`, and this plan with no untracked artifacts.
- [ ] **Step 59: Create one signed twelfth-round follow-up without rewriting history, build a fresh twelve-commit review package, and require fresh independent specification PASS followed by fresh quality PASS before any push.**
- **Twelfth independent specification verdict:** FAIL, 0 Critical / 1 Important / 1 Minor. The prior endpoint-before-process restart bug is closed, and the graceful Active→Unknown gate remains independently GREEN. The new Important is destructive force-kill authorization: restart treats any live number from the pidfile as the old daemon PID even when no control endpoint exists. A stale pidfile or PID reuse can therefore make `restart` wait 30 seconds and SIGKILL an unrelated local process; the parent behavior only failed `already running`. An isolated paused-time negative probe observed `forced_count = 1` instead of zero. The existing control `Status` command already returns the serving daemon's real process ID but is not consulted. Conversely, when the endpoint is reachable but the pidfile is absent, the candidate falls back to endpoint-only completion and can reproduce the original early-release race. The disclosed `source_protocol` migration remains Minor.
- **Thirteenth-round boundary:** Separate process waiting from destructive authority. Before Stop, a reachable control endpoint must return its actual daemon PID through `Status` (or equivalent authenticated control response); only that control-confirmed PID may authorize force-kill after the 30-second grace period. A live pidfile PID may still conservatively keep restart pending when the endpoint is already gone, but pidfile-only evidence can never authorize a signal. When control is reachable and the pidfile is missing/stale/different, the control-confirmed PID remains the wait/kill target while PID-file deletion stays exact-match only. Status failure must fail closed or use a non-destructive bounded fallback; it must not silently promote the pidfile to kill authority.
- [x] **Step 60: RED-test authenticated versus unauthenticated restart ownership.** Preserve the independent stale-live-pid/endpoint-absent negative: after the full grace period, no signal may be requested and restart must return a safe not-ready outcome. Add reachable-control/missing-pidfile and reachable-control/stale-different-pidfile cases proving the actual Status PID is awaited and is the only possible force-kill target. Cover Status error/unexpected response with no destructive fallback, natural exit, exact PID-file cleanup, and the existing endpoint-before-process sequence. Permanent behavior tests now execute the real control phase and verify the exact Status→Stop command sequence. They cover pidfile-only timeout with zero signals, missing and different pidfiles, natural exit, exact Status-PID force targeting, zero/Error/unexpected/transport Status failure with only one non-destructive command, cleanup bounds, and exact PID-file removal.
- **Thirteenth-round RED evidence:** On exact `a6ac00c9`, `restart_pidfile_only_target_times_out_without_force_kill` advanced paused time through the full grace period with an absent endpoint and a stale live pidfile target. The gate correctly remained not-ready, but the destructive authorization assertion failed 0/1 with exit 101: `forced_count` was one rather than zero (`pidfile-only evidence authorized a destructive signal`). This is the independent reviewer's behavioral failure made permanent before production changes.
- [x] **Step 61: Implement control-authenticated restart PID selection.** Query Status before Stop whenever the endpoint is reachable, validate the returned daemon PID, and carry an explicit kill-authority bit/source into the release gate. Pidfile evidence may extend waiting but never grant kill authority. After a verified PID times out, kill only that PID, require PID-dead plus endpoint-free, and remove the pidfile only if it still exactly matches. Preserve the existing Stop response, 30-second grace, two-second cleanup, missing-endpoint conservative wait, and fail-closed graceful reconciliation. GREEN introduces a private `RestartTarget { pid, kill_authorized }`. A reachable endpoint must return a nonzero Status PID before Stop; Status transport/error/unexpected/zero-PID outcomes fail closed before Stop/start/force. An absent endpoint may supply a live pidfile wait target only with `kill_authorized = false`; timeout returns not-ready without signaling. Only the control-confirmed PID can cross the existing force-kill boundary.
- [x] **Step 62: Rerun authenticated/unauthenticated restart, graceful-shutdown/reviewer probes, Task 11 full gates, and all static audits.** Restart passed 10/10; outer daemon passed 2/2; reviewer SQLite drain passed 1/1; SDK shutdown passed 4/4; continuation passed 84/84; application passed 1,195/1,195 with 9 skipped; SDK passed 777/777 with 2 skipped; fresh all-feature workspace nextest passed 2,493/2,493 with 11 skipped. The first App run hit an unrelated Fleet Git empty-revision concurrency flake; its exact isolated control passed 1/1 before the fresh App and workspace passes. Formatting, `git diff --check`, strict workspace all-feature/all-target Clippy, dist-helper (50 providers / 49 models), task-independence/privacy/no-014/runtime-artifact audits pass. The pre-commit tree contains only `apps/bitrouter/src/main.rs` and this plan with no untracked artifacts.
- [ ] **Step 63: Create one signed thirteenth-round follow-up without rewriting history, build a fresh thirteen-commit package, and require fresh independent specification PASS followed by fresh quality PASS before any push.**
- **Thirteenth independent review verdicts:** specification PASS, 0 Critical / 0 Important / 1 disclosed compatibility Minor; fresh quality FAIL, 0 Critical / 2 Important / 2 Minor. First Important: a resumed request substitutes the decrypted provider-native continuation id into the outbound body, but an upstream 400 can echo that value; current error normalization preserves it in `UpstreamBadRequest`, public HTTP rendering, and ordinary settlement error context. An independent dynamic probe confirmed all three stages. Second Important: Status authenticates a PID number only at one instant; the old daemon can exit during the 30-second wait and the PID can be reused before the numeric `kill -9`, so the replacement process becomes the destructive target. The new production stream guard also added `expect`/`panic!`, violating the repository's explicit no-panic rule (Minor). The required `source_protocol` public/serde migration remains the disclosed Minor.
- **Fourteenth-round boundary:** No provider-native continuation identity may survive into any error, settlement, event, log, trace, or HTTP boundary. Scrubbing must be request-local and structural: retain the public gateway continuation handle where safely available, otherwise suppress/redact; cover JSON objects/arrays and plain-text provider errors without a global secret registry. Numeric PID liveness can never provide stable destructive identity across time. Remove automatic restart force-kill rather than signal an unverifiable process: restart may wait up to the existing 30 seconds for natural exit and then return an explicit safe error; an operator's separate external force-kill remains the only hard-crash boundary. Finally, replace every Task 11 production `expect`/`panic!` state transition with fallible/no-panic control flow while preserving exactly-once settlement/drop behavior.
- [x] **Step 64: RED-test continuation identity scrubbing on upstream errors.** Through the real continuation resume and HTTP executor path, make an upstream 400 echo the substituted native id inside nested JSON and plain text. Require the outbound upstream request to contain the native id, while the returned `BitrouterError`, public HTTP body, ordinary settlement recorder, logs/events/traces, and serialized diagnostics contain no native id or credential. Where the public `brc_...` handle is available, replacement may expose only that handle. Cover streaming and non-streaming non-2xx paths, a successful HTTP stream whose SSE error event echoes private wire values, transport diagnostics that contain an authenticated query URL, truncation, percent-encoding, multiple occurrences, credentials before and after auth refresh, and provider-controlled unknown SSE type names that previously entered DEBUG logs before executor scrubbing.
- **Fourteenth-round privacy RED:** A real `ContinuationRuntime` resume through `HttpExecutor` reached a WireMock provider whose 400 nested JSON repeated both the substituted native parent id and the effective credential. `resumed_http_error_scrubs_native_parent_and_credential_before_public_surfaces` failed at the ordinary settlement recorder assertion: its `UpstreamBadRequest` message and nested value retained multiple native-id and credential sentinels. The failure is on the actual resume/error/settlement path, not a helper-only or setup failure.
- [x] **Step 65: RED-test PID reuse safety and no-panic production state transitions.** Simulate the Status-authenticated PID exiting and the same number becoming live again during the grace period. Require restart to emit no signal and return a bounded explicit error after 30 seconds. Assert no Task 11 production diff contains `.expect`, `.unwrap`, or `panic!`; exercise invalid/repeated stream-guard state transitions and require typed errors plus exactly-once settlement rather than process panic.
- **Fourteenth-round PID RED:** `restart_pid_reuse_timeout_never_signals_the_reused_process` simulated the authenticated old daemon exiting between conservative polls and the same numeric PID immediately becoming live for a replacement. After 30 seconds, the existing restart path still invoked the force callback; the assertion failed with `forced_count` one rather than zero at the intended timeout boundary. This is the quality finding's numeric-identity TOCTOU reproduced under paused time.
- **Fourteenth-round no-panic RED:** `invalid_stream_guard_processor_access_returns_without_panicking` deliberately exercised processor access after finalization under `catch_unwind`. The candidate panicked at `pipeline.rs` with `stream guard used after finalisation`, then failed the typed/no-panic behavior assertion. This confirms the quality Minor on a production state transition before replacing the four new `expect`/`panic!` sites.
- [x] **Step 66: Implement request-local error scrubbing, non-destructive restart timeout, and fallible stream-guard transitions.** Capture the public/native wire substitution pair without adding a public constructor break; capture both decoded and exact encoded wire credentials across every auth attempt; recursively replace private values in parsed upstream JSON strings and plain text before classification; and scrub transport, decoder, invalid-terminal, and parse failures before any public/settlement boundary. Delete numeric automatic force-kill authorization and return a stable timeout diagnostic without signaling. Convert stream processor/context extraction and state access to `Result`/safe option handling; preserve drop compensation and delivery authorization. GREEN now seeds the effective static/override key before auth construction, accumulates actual sensitive header/query values for every attempt (including decoded and raw encoded forms), recursively scrubs error JSON/text before classification, and scrubs auth-build/refresh, HTTP transport/body, successful-HTTP SSE decoder/transport, invalid-terminal, and parse errors without changing the existing decoder status classification. Raw unknown provider event/block names are no longer recorded in Responses or Messages DEBUG logs. Restart retains the 30-second conservative wait but contains no automatic signal path. Stream-guard access/extraction is fallible and restores owned state on invalid transitions so `Drop` retains settlement responsibility. Independent focused gates pass privacy 5/5, scrubber 7/7, stream outcome 9/9, restart 8/8, main binary 32/32, and full continuation 89/89; diff check and task/private/benchmark metadata audits pass.
- [x] **Step 67: Rerun privacy echo, restart reuse, stream lifecycle, continuation, graceful-shutdown/reviewer probes, Task 11 full gates, panic/source audits, and every static audit.** Record exact counts and require a clean tree. Independent exact-tree gates pass resumed privacy 5/5, scrubber 7/7, stream outcome 9/9, restart 8/8, complete main binary 32/32, and complete continuation 89/89. All-feature App nextest passes 1,198/1,198 with 9 skipped; all-feature SDK nextest passes 784/784 with 2 skipped; all-feature workspace nextest passes 2,503/2,503 with 11 skipped. The first sandboxed SDK run reached 169 passes before the existing local HTTP timeout test failed only because loopback bind was denied; the authorized full rerun produced the 784/784 result. `cargo fmt --all -- --check`, `git diff --check`, strict workspace all-feature/all-target Clippy, and dist-helper (50 providers / 49 models) pass. The Task 11 production diff contains no added panicking `.unwrap()`, `.expect`, or `panic!`, no automatic restart force-kill symbol/path, and no task/benchmark/prompt/model/workflow/case/private metadata or migration 014. The pre-commit tree contains only the six intended source files plus this plan, with no untracked artifacts.
- [ ] **Step 68: Create one signed fourteenth-round follow-up without rewriting history, build a fresh fourteen-commit package, and require fresh independent specification PASS followed by fresh quality PASS before any push.**
- **Fourteenth independent specification verdict:** FAIL, 0 Critical / 1 Important / 1 Minor. The continuation-native-id and actual known wire-credential scrubbing, non-destructive restart timeout, and fallible stream-guard corrections are independently closed. The new Important is the opaque dynamic-auth failure boundary: a public `AuthApplier` may return arbitrary error text from `prepare_body`, `apply_with_authority`, `refresh_after_unauthorized`, or `continuation_authority_proof` before a successful request exposes the plugin's credential to the request-local scrubber. Those errors currently reach normal settlement, observers, logs, and callers unchanged. An isolated exact-candidate probe registered a dynamic applier whose pre-wire apply error contained `dynamic-prewire-token-private-sentinel`; the returned `BitrouterError::Internal` still contained that sentinel. The disclosed `source_protocol` source/serde migration remains Minor. Quality review and every push remain blocked.
- **Fifteenth-round boundary:** Treat every diagnostic returned by an opaque authentication extension as untrusted at the SDK/runtime boundary. Replace it with fixed, operation-specific, credential-free diagnostics before it can enter routing, execution, settlement, observation, logging, or HTTP rendering. Keep the original error only inside the extension call; do not guess secret values, build a global secret registry, inspect task/request semantics, or weaken successful request authority proof and wire-auth validation. Built-in and third-party appliers retain the same success interfaces and typed authority values; only failure diagnostics are deliberately normalized.
- [x] **Step 69: RED-test all opaque auth-extension failure exits through production callers.** Register a task-neutral malicious dynamic `AuthApplier` that returns a distinct private sentinel from each of `prepare_body`, `apply_with_authority`, `refresh_after_unauthorized`, and `continuation_authority_proof`. Exercise non-streaming and streaming execution plus mapped continuation routing. Require the original candidate to expose each sentinel through at least one returned/settled error, then require the permanent test to scan caller errors, public HTTP bodies/SSE, ordinary settlement recorders, tracing logs/events, and observer/export payloads and find none. Include a successful dynamic-auth control proving request authority, effective scheme/principal, refresh success, and provider dispatch are unchanged.
- **Fifteenth-round RED evidence:** On exact signed candidate `45a4dd2f`, the new executor matrix ran `prepare_body`, `apply_with_authority`, and `refresh_after_unauthorized` failures in both streaming and non-streaming mode; after an authorized loopback rerun for the 401 refresh cases, it failed at the first privacy assertion because the caller error contained `opaque-prepare-private-sentinel`. A real Pipeline execution in both modes then failed because caller and ordinary settlement diagnostics contained `opaque-apply-private-sentinel`. Finally, an encrypted active continuation resolved through the real `ContinuationRuntime` and failed because the route caller received `opaque-authority-private-sentinel` from `continuation_authority_proof`. These are assertion-level behavioral REDs at production callers, not compiler, fixture, or helper-only failures; the initial unprivileged refresh run's only failure was the expected local-bind `PermissionDenied`.
- [x] **Step 70: Normalize opaque auth-extension failures at their owning boundaries.** Map body preparation, initial request authentication, unauthorized refresh, and continuation-authority resolution failures to fixed operation-specific `BitrouterError` diagnostics without interpolating the opaque source error. Centralize each normalization as close as possible to the extension invocation so every caller is covered before request-local value scrubbing; keep ordinary upstream HTTP/decoder/status classification and successful dynamic authority transport unchanged. Do not retain, clone, log, serialize, or attach the opaque source text anywhere outside the immediate call frame.
- **Fifteenth-round focused GREEN:** `HttpExecutor` now discards opaque error text at the three dynamic execution calls and emits fixed body-preparation, initial-authentication, or refresh diagnostics; `AuthAppliers` applies the same rule to both legacy and typed continuation-authority resolution before any runtime caller can wrap the error. Success values and interfaces are unchanged. The six-mode executor failure matrix passes 1/1; Pipeline caller/settlement/observer coverage passes 1/1; mapped encrypted continuation authority failure passes 1/1. A separate public Responses test was mutation-proved RED by temporarily restoring raw apply-error propagation, then GREEN after the normalization: both non-stream and stream-request HTTP bodies, ordinary settlement snapshots, and INFO logs contain no private sentinel, and the provider receives zero requests. Positive non-stream/stream 401 refresh controls pass 2/2 and stable dynamic authority across restart passes 1/1. Full gates remain pending.
- [x] **Step 71: Run focused auth-error privacy and positive-authority tests, then every Task 11 exact-tree gate and static audit.** Focused opaque-error coverage passes 1/1 for the six-mode executor matrix, 1/1 for Pipeline caller/settlement/observer propagation, 1/1 for mapped continuation authority, and 1/1 for public Responses non-stream/stream-request bodies, settlements, INFO logs, and zero provider dispatch. Positive dynamic-auth controls pass 2/2 for non-stream/stream 401 refresh and 1/1 for stable authority across restart; the complete continuation suite passes 91/91. Complete all-feature SDK nextest passes 786/786 with 2 skipped, App nextest passes 1,200/1,200 with 9 skipped, and the final serial all-feature workspace nextest passes 2,507/2,507 with 11 skipped. Two earlier parallel workspace attempts each had one infrastructure-only GPG failure: the fleet MCP review roundtrip could not lock the sandbox keybox, and a substrate branch-name test reported GPG `Cannot allocate memory`; each exact isolated authorized control passed 1/1 before the complete serial workspace run passed. `cargo fmt --all -- --check`, `git diff --check`, strict workspace all-feature/all-target Clippy, and dist-helper pass; the schema remains current at 50 providers / 49 canonical models. Static audits find only the five expected tracked files with no untracked file, migration, runtime artifact, production opaque sentinel, task/benchmark/provider-specific runtime metadata, added production panic, or automatic force-kill path. The tree is intentionally dirty only until the signed Step 72 commit.
- [ ] **Step 72: Create one signed fifteenth-round follow-up without rewriting history, build a fresh fifteen-commit review package, and require a new independent specification PASS followed by a separate fresh quality PASS before any push.**

## Task 12: Bound prefix-correlation work and index its lookup

**Files:**
- Modify: `apps/bitrouter/src/trajectory/canonical.rs`
- Modify: `apps/bitrouter/src/trajectory/store.rs`
- Modify: `apps/bitrouter/src/db/migration/m20240101_000012_create_trajectory_ledger.rs`
- Test: canonicalization complexity tests and migration/query-plan tests

**Interfaces:** Canonicalization serializes each typed turn once and incrementally hashes the canonical JSON array. It emits at most `MAX_ANCESTOR_PREFIX_DIGESTS = 256` newest prefix digests; older omitted ancestry fails conservatively to incomplete rather than causing unbounded work. Store resolution performs one indexed owner/digest lookup and restores newest-prefix priority in memory.

- [ ] **Step 1: Write equivalence and work-bound RED tests.** Compare the new incremental result against `serde_json::to_vec(CanonicalPrefix)` for literal small prompts. For 32/64/128/1024-turn prompts, instrument serialized bytes/hasher updates and assert linear input processing plus a hard maximum of 256 emitted prefixes.
- [ ] **Step 2: Add migration RED coverage.** Require composite `idx_trajectory_requests_owner_full_input_digest (owner_user_id, full_input_digest)` on SQLite, Postgres, and MySQL SQL generation.
- [ ] **Step 3: Implement incremental canonical-array HMAC.** Serialize the fixed object prefix and every `CanonicalTurn` once, clone/finalize HMAC state only for the newest bounded boundaries, and produce a full-input digest/byte count byte-for-byte compatible with the existing v1 contract.
- [ ] **Step 4: Replace per-prefix queries with one indexed membership query.** De-duplicate matches, choose the longest supplied prefix, and return `Ambiguous` when that digest maps to multiple episodes.
- [ ] **Step 5: Prove the SQLite query plan uses the composite index and run large-history correlation tests.** No prompt text or unkeyed digest may enter logs or storage.
- [ ] **Step 6: Run canonical, store, migration, correlation, cross-protocol, and privacy tests until GREEN.**
- [ ] **Step 7: Commit `perf(trajectory): bound prefix correlation`.**

## Task 13: Correct outbox delivery audit time and drain index

**Files:**
- Modify: `apps/bitrouter/src/trajectory/publisher.rs`
- Modify: `apps/bitrouter/src/trajectory/store.rs`
- Modify: `apps/bitrouter/src/db/migration/m20240101_000012_create_trajectory_ledger.rs`
- Test: publisher timing/restart and migration/query-plan tests

**Interfaces:** `delivered_at` is the UTC time at which Eval admission succeeds, not `created_at`. Global drain uses `idx_trajectory_outbox_delivery_order (delivered_at, attempts, created_at, outbox_id)`; owner-scoped inspection remains owner-filtered.

- [ ] **Step 1: Write a RED delivery-time test using distinct deterministic timestamps.** Assert delayed admission records the later successful-delivery time and exact retry does not rewrite it.
- [ ] **Step 2: Write migration/query-plan RED coverage for the global drain order.** Require an index whose leading column is `delivered_at` and whose remaining columns match the query order.
- [ ] **Step 3: Capture the admission-success timestamp and pass it to `mark_outbox_delivered`.** Preserve idempotency and restart behavior.
- [ ] **Step 4: Add the global delivery-order index and retain any separate owner inspection index only when query-plan evidence needs it.**
- [ ] **Step 5: Run publisher, outbox, migration, retention, and Eval Exchange tests until GREEN.**
- [ ] **Step 6: Commit `fix(trajectory): correct outbox delivery audit`.**

## Task 14: Validate the exact PR tree and record benchmark evidence

**Files:**
- Modify: this plan's checkboxes as tasks complete
- Modify: the single Draft PR description/checklist throughout execution
- Modify: implementation/docs only for defects found by validation

- [ ] **Step 1: Generate a fresh full-diff review package and require an independent reviewer to close every Critical/Important finding.** The review at `c199f9a9` found 0 Critical, 5 Important, and 2 Minor; its earlier green gates are invalidated by Tasks 8-13 and cannot be reused.
- [ ] **Step 2: Run focused tests after every remediation task and record the exact command/result in the PR.**
- [ ] **Step 3: Run `cargo fmt --all -- --check`.**
- [ ] **Step 4: Run `cargo clippy --all-features --all-targets -- -D warnings`.**
- [ ] **Step 5: Run `cargo nextest run --all-features`; if nextest is unavailable, run `cargo test --all-features` and record the substitution.**
- [ ] **Step 6: Run `cargo run -p dist-helper -- check`.**
- [ ] **Step 7: Freeze and run a fresh, independent Terminal-Bench 2.1 short13 mechanism lineage.** Use one trial per predeclared case, fresh run IDs/paths/ports/policy database/artifacts, explicit AWS identity and quota proof, non-evaluation canaries, exact request-ID joins, authoritative four-bucket settlement, exact-tag cleanup, and `control+r1+r2[+r3]` only as frozen before launch. Never read, copy, mutate, or reuse the sibling benchmark process or its run-scoped artifacts.
- [ ] **Step 8: Compare every declared point.** Report task outcome, request/turn count, time-to-outcome, exact authoritative actual/notional cost separately, recovery recurrence, and model-tier sequence. A one-trial short13 result is mechanism evidence only, never a stable model ranking or public score.
- [ ] **Step 9: Classify the result honestly.** A semantic regression blocks readiness. A pass with unbounded turn/cost inflation also blocks readiness. Missing authoritative cost remains `unknown`; it is not zero and does not erase a decisive trajectory-count regression.
- [ ] **Step 10: Update the PR description with final scope, commit map, review evidence, test evidence, benchmark evidence, remaining risks, compatibility notes, and rollout/rollback instructions. Mark ready for review only when every required gate is green.**
- [ ] **Step 11: Commit final validation/docs fixes using a scoped conventional title; do not create a second PR.**

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
| Existing behavior compatibility | 4, 6, 7, 9 | guard-disabled legacy lock/config and request-Eval matrix |
| Contradictory ancestry evidence | 2, 7, 8 | native-wins/incomplete conflict, guard, restart, and replay tests |
| Terminal lifecycle on routing errors | 5, 10 | SDK route-failure settlement plus restart/outbox/prune tests |
| Streaming Responses continuation | 2, 7, 11 | stable stream ID and native Responses-to-Responses continuation |
| Bounded authenticated request work | 2, 12 | incremental-HMAC work counters, bounded prefixes, and indexed query plan |
| Accurate outbox audit and drain | 5, 6, 13 | delivery-time idempotency and global pending-order query plan |

## Single-PR Delivery Protocol

1. The first commit contains this reviewed phase plan and immediately creates one Draft PR stacked on `codex/policy-effect-v2-short13`.
2. Every task lands as one or more scoped conventional commits on the same branch and same PR. No task creates a separate PR.
3. After every push, update the PR checklist, current implementation status, test evidence, and any changed risk/scope. The PR description is the live source of progress truth; this document is the detailed execution contract.
4. If research or implementation invalidates an assumption, update this plan and the PR description in the same commit that changes direction. Do not silently drift.
5. The concurrent benchmark remains external evidence. Its results are copied into the PR's validation section once authoritative; the benchmark worktree and running session are not modified from this branch.
6. Keep the PR Draft until the full validation and benchmark gates are satisfied. A passing task with material trajectory/cost inflation is not sufficient for readiness.
