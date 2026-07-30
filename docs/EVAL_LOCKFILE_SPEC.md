# Eval-Compiled Policy Lock Specification

**Status:** Proposed

**Date:** 2026-07-30

**Scope:** BitRouter OSS policy routing, legacy adequacy state migration, and the
future external Eval Exchange boundary

## 1. Decision

The active policy lock is the only source of truth for live **policy route
intent**. A request may be assigned to a different execution endpoint only by
an operational fallback rule declared by the active lock or main config; an
adequacy, reward, or evaluation database may not silently change the selected
policy tier.

Evaluation implementations remain outside the BitRouter process. BitRouter
owns the versioned evaluation contract, immutable evidence snapshots, result
admission, candidate compilation, validation, and atomic publication.

The resulting lifecycle is:

```text
active lock -> route -> observe -> external evaluate -> evidence ledger
            -> compile candidate -> validate -> publish -> new active lock
```

Raw evidence is not embedded in the YAML policy lock. The lock contains the
compiled route table, decision-relevant evaluation summaries, certificate
digests, and the content digest of the exact evidence snapshot used to build
it. The raw ledger is required to continue learning or perform a full audit,
but not to serve the published policy.

## 2. Problem

The current adaptive runtime has two independent authorities:

1. `policy-lock.yaml` supplies static tiers, route entries, guardrails, and
   adequacy thresholds.
2. The configured database supplies persisted pins, exploration counters,
   learned cheap locks, task-success counts, and provider reliability events.

In adaptive mode, database state can override the active lock in both
directions:

- a pin changes a static economy route to the escalation tier;
- a learned lock or exploration trial changes a static strong route to the
  explore tier;
- a reliability circuit changes a selected non-strong route to the escalation
  tier.

Consequently, copying the current lock without the source database does not
reproduce the source runtime. The current `policy evolve` command only
materializes qualified positive locks. It does not compile negative pins,
provider reliability, evaluation provenance, or existing economy routes that
should be returned to strong. It is intentionally add-only, so it cannot make
the lock a complete snapshot of adaptive behavior.

The current reward store is also not a general evaluation store. It compresses
evidence immediately into a small state machine:

- request completion counts;
- consecutive adequate trials;
- binary learned locks;
- distinct successful task counts;
- binary safety pins;
- provider reliability events.

It does not retain a versioned outcome vector, evaluator identity, rubric,
authority, confidence, conflict, cohort, holdout role, or complete evidence
provenance.

## 3. Goals

### 3.1 Phase 1 goals

1. The same BitRouter revision, main config, active lock, preset, request, and
   declared operational inputs produce the same policy tier selection whether
   the database is empty, populated, copied, or absent.
2. Changing learned database rows alone cannot change live policy selection.
3. Positive learned locks and negative pins can be compiled into an explicit
   candidate route table.
4. The compiler is deterministic: identical lock bytes, evidence snapshot,
   compiler version, and compiler config produce byte-identical candidates.
5. A published lock can be shipped without the training database and retain
   its policy tier behavior.
6. Existing `policy.mode` remains process-owned. The lock cannot activate,
   freeze, or publish itself.
7. Existing v1 locks remain readable, and legacy adaptive state is not silently
   discarded during migration.

### 3.2 Phase 2 goals

1. Terminal-Bench, CI, human review, bitrouter-agent, and enterprise private
   evaluators submit the same versioned result contract.
2. Evaluators run outside BitRouter and never write policy tables directly.
3. Episode-level outcomes do not need to be copied into one outcome row per
   model request.
4. Decision-level credit is optional and explicit. Missing credit uses a
   conservative attribution rule rather than broadcasting full terminal reward
   to every route transition.
5. Only admitted evaluation records can enter a frozen evidence snapshot used
   by the candidate compiler.

## 4. Non-goals

The first implementation does not:

- make `policy-lock.yaml` a complete BitRouter deployment file;
- move providers, credentials, presets, variants, listen configuration, or
  `policy.mode` into the lock;
- embed raw prompts, messages, tool calls, diffs, tests, or evaluator output in
  YAML;
- execute Python, Node, WASM, webhook, or LLM evaluator plugins inside the
  router process;
- schedule distributed evaluator workers;
- implement learned causal credit assignment;
- auto-publish candidates without an explicit operator action;
- treat provider unavailability as semantic model inadequacy.

The target environment still needs a compatible `bitrouter.yaml`, provider
credentials, and referenced models. "Ship one lock" means ship one **policy
artifact**, not one complete server deployment artifact.

Preset selection keeps the previously agreed virtual-model syntax. `@auto` is
the default preset and variants such as `@auto:cost` select a named policy
objective. The lock stores canonical preset names without the leading `@`;
request parsing and preset resolution remain configuration concerns.

## 5. Source-of-truth boundaries

| Concern | Authority |
| --- | --- |
| Active policy tier and capability decision | Active policy lock |
| Whether publication is allowed | `bitrouter.yaml` process mode |
| Provider credentials and endpoints | Main config and credential stores |
| Live provider availability | Operational reliability state, constrained by declared fallback rules |
| Historical observations and evaluations | Append-only evidence ledger |
| Why a route was published | Lock certificate and evidence root |
| Candidate-to-active history | Atomic promotion record and parent lock digest |

The phrase "lock is the only source of truth" applies to policy route intent.
Live availability remains an input to execution. Reliability state may trigger
only fallback behavior declared by configuration; it cannot create a new
semantic route or teach a route key.

Every decision record must separate:

```text
policy_decision   = the tier/model selected from the active lock
execution_result  = the endpoint/model actually served after operational fallback
```

This prevents an outage fallback from being reported as learned policy
optimization.

### 5.1 SQLite disposition

SQLite may remain the default local persistence implementation, but it no
longer has any special authority:

| SQLite content | Allowed runtime use |
| --- | --- |
| Request and decision observations | Append and export only |
| Legacy adequacy pins/locks/counters | Read only by the migration compiler |
| Evaluation subjects and results | Append, admit, snapshot, and compile only |
| Operational provider health | Select only a fallback edge already declared by configuration |
| Live semantic policy selection | Never read |

Deleting, copying, or replacing SQLite therefore cannot change the policy tier
chosen for a request. It may remove audit or future-learning inputs, and it may
change whether an operational fallback is currently necessary, but those are
reported separately from policy intent.

## 6. Target runtime behavior

### 6.1 Frozen mode

`policy.mode: frozen`:

- routes every request from the active lock;
- records observations and imported evaluations;
- permits dry-run candidate compilation and separate candidate export;
- rejects active lock replacement;
- never consults learned semantic state for a live decision.

### 6.2 Adaptive mode

`policy.mode: adaptive`:

- routes every request from the active lock, exactly like frozen mode;
- records observations and imported evaluations;
- permits deterministic candidate compilation;
- permits an explicit, validated, atomic `--apply` operation;
- does not allow a database row to override the active lock between
  publications.

Adaptive therefore means "publication is authorized", not "mutable database
state participates in the hot path". Automatic promotion may be added later,
but must use the same compile, validate, and atomic publish boundary.

## 7. Policy lock v2

Phase 1 introduces `lockfileVersion: 2`. The route map stays compact and easy
to diff. Certificates are stored separately so the selector does not need to
parse raw evaluation records.

Illustrative shape:

```yaml
lockfileVersion: 2
artifact:
  parent_digest: "sha256:..."
  evidence_root: "sha256:..."
  source_snapshot_time_unix_ms: 1785369600000
  migration:
    legacy_adequacy_digest: "sha256:..."
  compiler:
    id: "bitrouter-policy-compiler"
    version: "1"
    config_digest: "sha256:..."

policies:
  auto:
    key_strategy: agent_trace
    tiers:
      economy: "bitrouter:deepseek/deepseek-v4-pro"
      strong: "openai-codex:gpt-5.6-sol"
    routes:
      "agent_trace/v1|edit|normal": economy
      "agent_trace/v1|recovery|guarded": strong
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong, economy]
    fallbacks:
      economy: strong

certificates:
  "auto\u0000agent_trace/v1|edit|normal":
    selected_tier: economy
    source: evaluated
    eligible_episodes: 84
    independent_tasks: 31
    quality:
      baseline_pass_rate_ppm: 950000
      candidate_pass_rate_ppm: 940000
      delta_ppm: -10000
      lower_bound_ppm: -35000
    economics:
      normalized_cost_delta_ppm: -420000
    critical_violations: 0
    verdict: promote
    evidence_digest: "sha256:..."
```

The exact schema will use integer fixed-point values for portable,
deterministic serialization. The compiler must not insert wall-clock time,
random IDs, unordered maps, or environment-dependent paths. If human-readable
timestamps are needed, they come from records already included in the frozen
snapshot and do not affect compiler nondeterminism.

`source_snapshot_time_unix_ms` is an explicit compiler input, not the time at
which the compiler happens to run. It is used for time-dependent legacy rules
such as pin expiry. `migration.legacy_adequacy_digest` covers the canonical
contents of the sealed legacy tables and proves which pre-v2 learned state was
projected into the candidate.

### 7.1 Certificate requirements

Each non-operator route produced by the compiler has a certificate containing:

- policy and canonical route key;
- selected tier and compared baseline tier;
- source class (`legacy_adequacy_v1`, `task_native`, `human`, `enterprise`, or
  `agentic`);
- eligible episode and independent-task counts;
- available quality, cost, latency, and reliability summaries;
- hard-violation count;
- promotion verdict;
- evaluator/compiler config digests;
- evidence subset digest.

Operator-authored routes may use `source: operator` without fabricated scores.
Migrated legacy evidence uses `source: legacy_adequacy_v1`; missing metrics stay
absent rather than being reported as zero.

### 7.2 Digest behavior

The semantic lock digest covers route behavior, guardrails, fallback rules,
artifact lineage, and certificates. A certificate-only change therefore
produces a new policy artifact even if the selected routes are unchanged.

Serving requires only a valid lock and compatible main config. Full audit can
optionally verify `evidence_root` against a local or imported evidence bundle.
Missing raw evidence does not make an otherwise valid published lock inert.

### 7.3 Route ownership and candidate precedence

The certificate identifies each route as operator-owned or compiler-owned.
Candidate compilation uses this precedence:

1. hard guardrails declared in the lock;
2. explicit operator-owned routes;
3. admitted negative evidence affecting a compiler-owned route;
4. admitted positive evidence affecting a compiler-owned route;
5. the inherited compiler-owned route or policy default.

The compiler does not silently overwrite an operator-owned route. If negative
evidence conflicts with one, candidate generation returns a blocking conflict
that requires an explicit operator edit or override. A negative pin always
wins over a positive learned lock for the same compiler-owned route.

## 8. Legacy state compilation

The first compiler supports a one-time deterministic projection from the four
existing adequacy tables.

### 8.1 Positive state

A legacy route key compiles to the explore tier only when:

- the exploration row is locked;
- the canonical route key is valid;
- the policy namespace exists;
- the distinct semantic-success count meets the configured threshold;
- the opening-specific threshold is satisfied when applicable;
- no active negative pin for the same key wins.

### 8.2 Negative state

An active legacy pin compiles to the configured escalation tier. This may
replace an existing economy route in the candidate. Unlike the current
add-only `evolve`, the compiler must represent safety corrections explicitly.

Pin expiry is evaluated against a caller-supplied frozen snapshot time. The
compiler never calls the current wall clock while producing candidate bytes.
For `pin_cooldown_secs: 0`, a persisted pin remains active.

### 8.3 Exploration counters

`observed` and `adequate_trials` are recorded in the legacy certificate but do
not schedule live trials after publication. Continuing exploration happens by
creating an explicit experimental candidate or cohort, not by mutable hot-path
cadence.

### 8.4 Reliability events

Legacy reliability events are not compiled into semantic route promotions or
pins. They may be summarized for audit and used to initialize an operational
health report, but provider availability must not become permanent evidence
that a model lacks semantic capability.

### 8.5 Migration safety

A new binary must not silently ignore non-empty legacy learned state in an
adaptive deployment. The migration sequence is:

1. stop the old adaptive daemon;
2. freeze the database and active lock inputs;
3. compile and export a v2 candidate from the legacy state;
4. inspect and validate route changes, especially pinned economy routes;
5. atomically publish the candidate under adaptive mode;
6. start the new lock-only runtime;
7. archive the legacy database for audit.

If an adaptive startup detects legacy learned state that has not been
acknowledged by a matching `migration.legacy_adequacy_digest` in the active v2
lock, it fails closed with a migration command and makes no model request. The
legacy adequacy tables are sealed after migration; new observations use the
new evidence ledger rather than mutating those tables. Frozen v1 locks remain
serveable because their database state was already non-authoritative.

## 9. Eval Exchange boundary

Phase 2 adds external evaluation without changing the serving invariant.

### 9.1 EvalSubject

BitRouter creates an immutable, redacted subject for one request, episode, or
policy cohort. It includes stable request and decision IDs, policy/model epochs,
requested evaluation dimensions, evidence references, and an evidence digest.

### 9.2 EvaluationResult

An external evaluator submits:

- eval and subject IDs;
- the exact evidence digest it evaluated;
- evaluator kind, ID, version, and config digest;
- verdict and outcome vector;
- hard violations;
- confidence and evidence references;
- optional per-decision credit;
- an idempotency key.

Terminal-Bench becomes one adapter that produces this contract. Human CLI,
CI, bitrouter-agent, and enterprise private evaluators use the same contract.
No adapter may write an active lock or learner table directly.

### 9.3 Admission and conflict

BitRouter stores immutable results and produces an admitted evaluation record
only after schema, identity, evidence digest, evaluator authority, cohort, and
holdout checks pass. Conflicting authoritative results produce `disputed`, not
an averaged score. Agentic evaluation cannot override a task-native hard
failure by default.

## 10. CLI and compatibility surface

The intended first-stage surface is:

```text
bitrouter policy compile --legacy-state --output <candidate.yaml>
bitrouter policy check --config <bitrouter.yaml>
bitrouter policy diff <active.yaml> <candidate.yaml>
bitrouter policy evolve --apply --config <bitrouter.yaml>
```

The final command name may remain `evolve` for compatibility, but its
implementation becomes compile, validate, and atomic publish. It no longer
means "copy positive rows into an add-only route map".

The intended second-stage surface is:

```text
bitrouter eval list [--pending]
bitrouter eval show <eval-id>
bitrouter eval export <eval-id>
bitrouter eval submit <result.json>
bitrouter eval review <eval-id> (--pass | --fail)
bitrouter eval status
```

REST mirrors the same library operations:

```text
GET  /v1/evals?status=pending
GET  /v1/evals/{eval_id}
GET  /v1/evals/{eval_id}/evidence
POST /v1/evals/{eval_id}/results
```

CLI and REST are thin adapters over one eval library and must produce
semantically identical records.

## 11. Publication and rollback

Candidate compilation never mutates the active lock. Apply performs:

1. expected active digest comparison;
2. full v2 schema and cross-config validation;
3. referenced model/tier/guardrail validation;
4. certificate/evidence-root structural validation;
5. atomic write preserving file permissions;
6. runtime reload;
7. last-known-good retention if reload fails;
8. append-only promotion record containing parent and child digests.

Rollback restores exact prior lock bytes from the promotion chain. It does not
rerun evaluation or recompile evidence.

## 12. Error handling

- **Missing database:** serving continues from a valid active lock; compilation
  and full evidence audit report unavailable inputs.
- **Missing evidence blobs:** serving continues; `policy verify --evidence`
  reports incomplete audit coverage.
- **Invalid candidate:** active bytes remain unchanged.
- **Concurrent apply:** expected-digest mismatch rejects the later publisher.
- **Evaluator unavailable:** eval remains pending; active routing is unchanged.
- **Stale evaluation digest:** result is retained as rejected evidence and is
  ineligible for compilation.
- **Conflicting evaluation:** result becomes disputed and cannot promote a
  route without an explicit adjudication policy.
- **Legacy adaptive state not migrated:** startup fails before serving traffic.
- **Operational circuit open:** execution follows only the declared fallback
  edge and records policy intent separately from fallback execution.

## 13. Security and privacy

- Raw messages, tool arguments, code, and evaluator output remain outside the
  lock.
- Evidence snapshots are redacted before exposure to an external evaluator.
- REST evaluation endpoints use the existing BitRouter authentication and
  tenant boundary.
- Evaluator identity and authority come from authenticated configuration, not
  self-declared JSON alone.
- Offline imports retain source and content digests and are not automatically
  trusted.
- A lock may be distributed publicly without distributing private evidence;
  the evidence root still permits later verification by an authorized holder.

## 14. Acceptance criteria

### 14.1 Phase 1

1. With the same config, lock, and request fixture, populated and empty learned
   databases produce identical decision-relevant policy fields in both modes.
2. Mutating a pin, exploration row, or semantic-success row without publishing
   a new lock cannot change the next selected tier.
3. A legacy learned lock compiles to an explicit economy route.
4. An active legacy pin compiles to an explicit strong route and wins over a
   positive lock for the same key.
5. Reliability events never compile into semantic promotions or pins.
6. Compiling the same frozen inputs twice produces byte-identical v2 lock bytes
   and digest.
7. Applying under frozen mode fails without changing active bytes.
8. Applying under adaptive mode atomically changes active bytes and reloads the
   new digest.
9. An invalid reload keeps the last-known-good route table.
10. A target installation with a compatible main config and no training
    database serves the same policy tier decisions as the publisher.
11. An unmigrated adaptive legacy database fails closed with an actionable
    migration message.

### 14.2 Phase 2

1. Terminal-Bench, human, mock agentic, and enterprise fixtures submit the same
   versioned result schema.
2. CLI and REST imports are idempotent and semantically identical.
3. Episode outcomes are stored once and optional decision credit remains
   explicit.
4. Held-out and disputed results cannot update training evidence.
5. An evaluator cannot mutate or publish an active lock.
6. A frozen evidence snapshot compiles deterministically into the same
   candidate and certificates.
7. Missing or unavailable evaluators never change live routing.

## 15. Implementation slices

Implementation is intentionally split so the source-of-truth fix ships before
the general Eval Exchange. Slices A through C form one migration milestone:
none of them is released as a selector cutover on its own.

### Slice A: Legacy compiler and lock v2

- add lock v2 parsing, canonical serialization, artifact lineage, migration
  digest, and certificates;
- snapshot and compile positive locks plus negative pins;
- keep reliability evidence out of semantic compilation;
- replace add-only evolution with deterministic candidate compilation;
- add a shadow comparison between current adaptive decisions and the compiled
  candidate before cutover.

### Slice B: Atomic promotion and migration preflight

- validate expected parent digest and referenced config/model contracts;
- publish atomically in adaptive mode;
- retain last-known-good and promotion history;
- detect non-empty unacknowledged legacy state and provide the exact migration
  command;
- prove a lock-only target with an empty database reproduces policy decisions
  before selector cutover.

### Slice C: Lock-only selector cutover

- remove semantic pins, learned locks, and exploration cadence from the live
  policy selector;
- retain lock route selection and declared guardrails;
- separate policy decision from operational fallback decision;
- keep process mode authoritative for publication;
- stop writing legacy adequacy tables and add regression tests proving database
  independence.

### Slice D: Eval Exchange records and CLI

- add immutable EvalSubject, EvaluationResult, and admitted EvaluationRecord
  stores;
- add authority, conflict, idempotency, cohort, and holdout admission;
- add `eval list/show/export/submit/review/status`;
- retain current workflow-state reward import as a compatibility adapter.

### Slice E: REST and reference adapters

- expose authenticated REST operations over the same eval library;
- add Terminal-Bench, shell/CI, human, and mock agentic fixtures;
- compile admitted evidence summaries and digests into lock certificates;
- keep worker scheduling and a bundled general agentic evaluator out of scope.

## 16. Required tests

The implementation plan must include:

- lock v1/v2 parser and deterministic serialization tests;
- empty/populated/database-absent routing equivalence tests;
- pin-versus-positive-lock migration tests;
- frozen/adaptive publication authority tests;
- concurrent publisher expected-digest tests;
- last-known-good reload and byte-exact rollback tests;
- reliability fallback classification tests;
- lock-only distribution end-to-end test;
- eval schema, idempotency, authority, conflict, and holdout tests;
- CLI/REST semantic-equivalence tests;
- compatibility tests for current `workflow-state apply-reward-feedback`;
- secret/redaction tests for exported evidence snapshots.

## 17. Rollout

1. Ship the v2 compiler, shadow comparison, and migration preflight without
   changing the active selector.
2. Require adaptive users with non-empty legacy state to compile and publish a
   v2 candidate.
3. Cut the selector over to lock-only behavior only after the migration and
   portability checks pass; keep frozen v1 locks readable and deterministic.
4. Publish the `@auto` router template as v2 after the same checks pass.
5. Add Eval Exchange CLI after the lock boundary is stable.
6. Add REST and external evaluator examples without making any evaluator a
   required runtime dependency.
7. Consider optional agentic evaluator packaging only after deterministic and
   human/enterprise adapters prove the protocol.
