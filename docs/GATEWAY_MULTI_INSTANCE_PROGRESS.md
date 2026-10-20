# Native gateway multi-instance progress

Implementation follows the [P1 contract](GATEWAY_MULTI_INSTANCE_SPEC.md) and the
[original P1 gates](BITROUTER_SERVICE_REFACTOR_PLAN.md#p1-acceptance).
Status: **in progress; no P1 delivery gate is accepted yet**.

Current P0 base (2026-10-20): `9cbe512d101e777470d113b0086ab9e331b3a638`.
[#984](https://github.com/bitrouter/bitrouter/pull/984) is merge ready and remains
unmerged: [CI](https://github.com/bitrouter/bitrouter/actions/runs/39421381228)
passed 57/57 jobs, all checks were 63 successful/8 configured skips, and no
review threads remained unresolved. This does not close any P1 gate.

## Baseline and scope (2026-10-19)

- Branch: `codex/gateway-multi-instance`, based on P0 PR #984 at
  `08b5dcc2e8f19bf7f273c40b9e939f7bd043ab44`.
- P0 was rechecked before branching: clean working tree, open non-draft PR,
  `CLEAN` / `MERGEABLE`, full CI 57/57 successful on the first workflow attempt.
  The same-branch P0 follow-up corrected four documentation status records only.
- Local SQLite remains the default. P1 targets a real PostgreSQL shared backend,
  without a new runtime crate, Redis requirement or distributed core/harness.
- Existing source seams: fresh database-backed virtual-key authentication;
  transaction-based trajectory correlation/guard append; application-owned
  `ConfigRoutingTable`, provider `CredentialStore`, metering recorder, encrypted
  continuation registry, pipeline finalization and HTTP delivery handshake.
- Current independent process tests and the immutable `d233944` P0 performance
  artifacts are baseline references, not proof of P1 behavior.

## Stage ledger

| Stage | Implementation | Verification | Independent review |
| --- | --- | --- | --- |
| 1 Authority foundation | Store/key foundation implemented; startup pending | Real PostgreSQL foundation passed | Store/key foundation approved |
| 2 Admission and execution | Private engine implemented; serving integration pending | Real PostgreSQL transaction/race tests passed | Private engine approved |
| 3 Gateway integration | Credential binding, admission context, dispatch and delivery accounting implemented; HTTP integration pending | Scoped protocol/executor and PostgreSQL regressions passed | Authentication/context/dispatch/custody substages approved |
| 4 Published authority | Immutable publication implemented; operator and runtime wiring pending | Real PostgreSQL publication and local parser regressions passed | Publication substage approved |
| 5 Trajectory authority | Pending | Pending | Pending |
| 6 Delivery and operations | Bounded custody implemented; transport/workers pending | Scoped custody/cancellation regressions passed | Custody approved; transport/worker implementation pending |
| 7 Aggregate acceptance and PR | Pending | Pending | Pending |

## Acceptance ledger

| Gate | Evidence required | Current status |
| --- | --- | --- |
| P1-A01 | Independent processes share tenant quota and actual account concurrency | Pending |
| P1-A02 | Cross-instance duplicate/conflict behavior and unknown status query | Pending |
| P1-A03 | Kill before dispatch, during inference/streaming and around finalization | Pending |
| P1-A04 | Competing finalization/outbox retries and truthful unknown states | Pending |
| P1-A05 | Auth/config/credential revisions, cross-instance continuation and backend loss | Pending |
| P1-A06 | Concurrent/alternating episode guards, history, cost, holds and key rotation | Pending |
| P1-A07 | Bounded overload, slow consumers and hot-tenant isolation | Pending |
| P1-A08 | Drain/rolling shutdown with truthful interrupted streams | Pending |
| P1-A09 | SQLite without shared infrastructure, dependency closure and measured performance | Pending |

Use exact revisions, feature profiles, command/log identities and failure
dispositions when closing these rows. In-process tests cannot close a
multi-process gate by themselves. Unknown usage is never counted as zero and a
lease timeout is never proof of stopped upstream work.

## Foundation evidence (2026-10-19, uncommitted candidate)

- Independent design review closed all seven original findings, including the
  shared-reader/exclusive-publisher fence needed in addition to serializable
  transactions. This approves the design, not runtime delivery.
- Public execution contracts: the two selected `bitrouter-ai` contract tests
  passed with all crate features; this was not a full workspace run.
- Real PostgreSQL 17 foundation tests use independently created pools and UUID
  namespaces in a dedicated local fixture database. They verify concurrent
  migration initialization, namespace separation, atomic rollback, shared key
  identity, retained correlation epochs, stale publication rejection and
  concurrent reader fencing of publication.
- Foundation review found connector TRACE credential exposure and pooled reads
  lacking server-side timeouts. Both were fixed and independently re-reviewed.
  Regression cases capture a sentinel password under TRACE, interrupt a write
  after taking its row lock, and block reads/scans using a separate table lock
  while proving recovery of the only connection in a one-connection pool.
- Command profile: Rust 1.99.0, `--locked -p bitrouter --no-default-features
  --features multi-instance --lib gateway::shared -- --include-ignored`.
  `/tmp/bitrouter-p1-foundation-review-tests.log`: **3 passed, 0 failed,
  0 ignored**. The PostgreSQL test also covers concurrent duplicate admission,
  independent tenant limits, revoked-key retries and worker drain admission.
- Runtime/startup configuration, public endpoints, durable finalization,
  recovery, OAuth refresh and full process acceptance remain in progress.
  These store-level tests do not close any P1 acceptance gate.

## Execution engine evidence (2026-10-20, uncommitted candidate)

- Atomic tenant/principal rate admission, tenant and actual-account concurrency,
  duplicate binding, revision-fenced dispatch, terminal evidence, recovery,
  explicit operator resolution and leased terminal outbox are implemented in the
  private shared engine. They are not yet attached to served inference.
- Independent review found and closed three races: terminal evidence now installs
  its episode projection barrier before releasing provider capacity; dispatch
  rechecks principal expiry after lock waits; recovery defers an individually
  failing record so unrelated tenants can progress.
- The final scoped engine review also approved semantic regressions for both
  observation/operator commit orders, competing resolutions, complete rollback
  after a late outbox collision, a failing-first recovery scan with batch size
  one, and episode barriers at their 256-entry bound.
- `/tmp/bitrouter-p1-terminal-boundaries.log`, same PostgreSQL/Rust profile as
  above: **3 passed, 0 failed, 0 ignored**, 7.87 seconds. The PostgreSQL case
  includes the transaction and interleaving campaigns; this count is not a
  multi-process gateway acceptance result.
- Configuration, OAuth refresh, HTTP/SDK integration, immutable publication,
  guard catch-up, workers and process-level acceptance remain unfinished.

## Refresh and mode validation evidence (2026-10-20)

- Explicit `gateway_state` configuration now validates authentication, PostgreSQL,
  namespace/key references and process bounds without resolving credentials.
  Assembly checks the feature boundary. Serving is not yet wired to this mode.
- OAuth account leases persist refresh intent before either ordinary or
  rejected-token refresh. Read leases release explicitly; uncertain refreshes
  remain fenced after expiry. A replacement commits encrypted state before
  exposure, and operator replacement fences stale refresh owners. A refresh
  finishing after its 60-second lease requires explicit account replacement.
- Independent review approved this subsystem and its local compatibility.
  `/tmp/bitrouter-p1-refresh-final-tests.log`: **3 passed**, 7.86 seconds;
  `/tmp/bitrouter-p1-mode-tests.log`: **1 passed**;
  `/tmp/bitrouter-p1-auth-regression.log`: **8 auth + 7 file-store tests passed**.
  The mode regression caught and fixed ignored fields on the local enum variant.
- Runtime wiring, process crashes and lost commit acknowledgements remain part
  of later acceptance. No P1 gate is closed by this subsystem review.
- A fresh remote check found P0 #984 conflicting after `main` advanced to
  `c215cb7`. Its old head remains CI-green, but is no longer merge ready.
  Integration is proceeding on the P0 branch in a separate checkout before
  this P1 branch is updated to the corrected base.

## Updated P0 base and immutable publications (2026-10-20)

- P0 integrated post-revert main `e972d5e` and was pushed on its original PR
  branch at `effc842f438144857a974eaaef8b2ac02afcac11`. P1 was rebased onto
  that head; its foundation checkpoint is now `cd3d385`. Full P0 hosted CI
  is still running at the time of this entry. Superseded Goals integration
  work is not part of this branch.
- Implemented encrypted immutable routing publications, compare-and-set head
  advancement under the exclusive authority fence, complete credential-account
  binding, and independent-replica reads of retained generations. Aliases refer
  to the same imported credential/resource authority. Failed preparation or
  missing credentials do not advance the active generation.
- Publication preparation uses resolved configuration without interpreting
  environment references again. It accepts embedded access policies and policy
  locks, rejecting process configuration, local file references, dynamic model
  discovery, inline provider credentials, native extensions and unsupported
  local spend/rate limits. It does not load per-worker files or credentials.
- Independent review found two issues and approved the fixes: validation errors
  now discard source diagnostics that could include secret values, and provider
  count/derivation depth are checked before recursive resolution. Regression
  cases include Display/Debug redaction, sub-megabyte chains of 33, 257 and 4,000
  providers, cycles and valid shallow inheritance.
- `/tmp/bitrouter-p1-publication-reviewed-tests.log`: **6 passed**, including
  the real PostgreSQL publication campaign, 7.87 seconds. The same minimal
  `multi-instance` test profile is used as the earlier foundation campaign.
  `/tmp/bitrouter-p1-publication-config-regression.log`: **101 passed** for
  existing configuration parsing. After the final native-extension guard,
  `/tmp/bitrouter-p1-publication-final-unit.log`: **3 passed**, 0.04 seconds.
  These results do not close a process-level
  P1 gate. Operator commands, serving snapshot installation, lifecycle wiring,
  shared trajectory reconciliation and final process acceptance remain pending.

## In-memory policy runtime preparation (2026-10-20)

- Added a feature-gated constructor for a complete published policy document.
  It shares the existing validated router construction, preserves each retained
  request snapshot and refuses local file reload before filesystem access.
  File-backed local construction retains its existing behavior.
- `/tmp/bitrouter-p1-published-policy-tests.log`: **64 passed**, 0.39 seconds,
  including distinct old/new runtime documents, an unrelated replacement file,
  missing bound policy, trajectory-mode mismatch and explicit-path rejection.
- Independent source review approved this helper substage. It is not yet wired
  into the served shared gateway and does not close a P1 delivery gate.

## Injected shared assembly (2026-10-20)

- P1 is now based on P0 follow-up `2240b7dc6e99fa8374076e33c1e92c64c8d344a6`.
  Rebased private foundation/publication/policy checkpoints are `ecb4079`,
  `503636d` and `80983eb`. P0's fresh hosted CI remains in progress.
- Added an internal shared constructor accepting the prepared PostgreSQL pool,
  auth registry, continuation/correlation keys, access policies and policy lock.
  It rejects a local injected backend before initialization and refuses ordinary
  local assembly of shared process configuration. It does not connect/migrate a
  database, discover local credentials/models or read a local policy lock.
- Local credential activation, bundled provider defaults, key generation,
  retention, continuation maintenance and local evaluation-outbox workers remain
  on the local path. The shared coordinator must explicitly own its maintenance
  and clock/authority fences; this helper does not provide that coordinator.
- Independent review found two initialization leaks: local-clock continuation
  maintenance and an unused filesystem-backed JSONL recorder. Both are now
  excluded in shared mode. Re-review approved these fixes, the injected-backend
  guard and preservation of local initialization behavior.
- All-features compilation passed. `/tmp/bitrouter-p1-shared-assembly-regression-final.log`:
  **41 assembly tests passed**, 0.21 seconds, including existing local trajectory,
  key-lifecycle and telemetry behavior. The original fixture compile failure
  (`Config::parse` instead of the existing free parser) remains recorded in
  `/tmp/bitrouter-p1-shared-assembly-regression.log`; it was corrected before the run.
- `/tmp/bitrouter-p1-shared-assembly-minimum.log`: the injected SQLite rejection
  test also passed under `--no-default-features --features multi-instance`, 0.01 s.
  These are scoped local regressions, not successful shared serving or process
  acceptance. Internal authority/runtime code is still being connected, so its
  unused-code warnings are not suppressed or represented as a final lint pass.

All P1-A01–A09 delivery gates remain pending. Actual shared authentication,
dispatch, durable settlement, trajectory catch-up, operator APIs, workers and
multi-process acceptance are still required.

## Shared authentication binding (2026-10-20)

- The branch is based on P0 `b25909a`; the prior direct-reader checkpoint is
  `ed4c89b`. P0's new hosted CI remains in progress and is tracked separately.
- Added an application-owned auth registry that maps published route accounts
  to imported shared slots. Every provider receives an explicit mechanism;
  inline credential overrides and unbound accounts fail closed. Existing static
  transports and subscription mechanisms retain their protocol behavior.
- Each credential transaction captures a request-local observation before OAuth
  spawns its owned refresh task. Awaited release records the exact read or
  committed replacement version. The authenticated request carries that version
  and resource pool, with a wire commitment covering destination, credentials
  and account headers. Executor reconstruction checks and preserves the binding,
  including providers without a stable continuation proof.
- Review found that config resolution erases inheritance and that Google Chat
  replay still requires inline static authority. Preparation now retains the
  authentication mechanism from the validated publication source, including
  multi-level subscription aliases. Shared publications explicitly reject
  `google_extensions: true`; local Google behavior remains unchanged. This
  compatibility limit is also recorded in the implementation contract.
- Independent re-review approved both fixes and found no remaining scoped
  correctness/security/compatibility blocker.
- `/tmp/bitrouter-p1-auth-binding-reviewed-tests.log`: **9 passed**, 8.098 s,
  zero retries; real PostgreSQL authority/refresh/publication/authentication
  campaign, wire proof tests, executor preservation and publication validation.
  `/tmp/bitrouter-p1-auth-binding-regression.log`: **172 passed**, 5.350 s,
  existing authentication, provider and executor regressions. Unselected tests
  are not claimed as an aggregate pass. The initial string-type compile error
  is retained in `/tmp/bitrouter-p1-auth-binding-tests.log`.
- The served HTTP admission/dispatch/settlement integration is still pending;
  this substage does not close any P1-A01–A09 gate or establish multi-process
  serving acceptance. The registry is consumed by its real-backend campaign
  while the serving snapshot coordinator is being connected.

## Original admission context (2026-10-20)

- The accepted transaction now returns the original locked API-principal and
  policy snapshot alongside its public execution view. A private request object
  carries that authority through pipeline-input cloning, detached execution and
  stream context creation. Inbound JSON cannot construct this typed authority.
- `AuthHook` uses the shared admission's original database authority, checks its
  execution/caller/credential binding and emits the original policy identity.
  Revalidation fences the current principal, tenant/config revision, owner and
  database-clock expiry without reserving another quota unit. A changed policy
  or revoked key fails rather than silently adopting a new snapshot.
- `/tmp/bitrouter-p1-admitted-context-tests-fixed.log`: **117 passed**, 16.999 s,
  including the real PostgreSQL context campaign and existing auth/context tests.
  The campaign deliberately gives the ordinary auth hook a disconnected local
  backend, checks cloned and streamed authority, detects request tampering and
  key changes from an independent connection, and verifies unchanged counters.
- `/tmp/bitrouter-p1-admitted-context-local.log`: **109 passed**, 5.487 s, under
  `--no-default-features`, with no shared backend requirement. All-features,
  all-targets `cargo check --locked -p bitrouter` passed in
  `/tmp/bitrouter-p1-admitted-context-all-targets.log`. Unselected tests and
  staged runtime dead-code warnings are not represented as final acceptance.
- Independent review approved this substage with no findings. The initial
  ownership/accessor compile errors remain in
  `/tmp/bitrouter-p1-admitted-context-tests.log`; they were fixed before validation.
  All process-level P1 gates remain pending.

## Direct credential reader release (2026-10-20)

- P1 is now based on P0 runtime source `6218709`; the shared assembly checkpoint
  rebased to `66dac32`. P0 hosted checks and the P1 serving integration remain
  separate requirements.
- Anthropic API-key resolution and Copilot exchange/cache paths now await the
  credential transaction's release. Invalid or missing credentials and exchange
  failures also release their read leases. A release failure prevents exposing
  authenticated requests or derived tokens. Copilot retains the transaction
  through exchange, preserving local account serialization and cache behavior.
- The regression store deliberately retains its lease after transaction drop,
  modeling the explicit-release requirement of persistent shared storage.
  Consecutive requests cover valid, invalid and absent credentials, successful
  exchange, cache hits, provider errors and release failures. Existing refresh,
  authority and provider regressions remain included.
- `/tmp/bitrouter-p1-read-lease-tests.log`: Rust 1.99, `cargo nextest run
  --locked -p bitrouter --no-default-features --features multi-instance
  --test model_auth --test model_providers --test model_anthropic
  --test gateway_anthropic_authority --test-threads 2 --retries 0
  --no-fail-fast`: **21 passed, zero skipped or retries**, 0.115 seconds.
  Independent review approved this substage. No process-level P1 gate is closed;
  shared provider dispatch, HTTP/SDK integration and all aggregate gates remain
  pending.

## Provider dispatch and observed terminal facts (2026-10-20)

- Every shared JSON, Decisions and SSE send now reserves its selected account
  and commits dispatch intent before provider I/O. The executor requires both
  typed admission and the exact authenticated credential revision. Redirects and
  hidden client retries are disabled. Local execution retains its existing path.
- Request-owned attempt guards retain bounded, content-free terminal and usage
  evidence across cancellation. Definite HTTP/provider rejection is distinct
  from malformed/truncated/transport-ambiguous work. Ambiguity blocks fallback
  and retains capacity; guards do not spawn detached database tasks. A later
  bounded owner still must retain and reconcile interrupted admissions.
- Shared generation, Decisions and SSE use the same 16 MiB decoded-response
  bound, honoring narrower native bounds. Shared input-count requests fail
  closed because distributing the core controller is outside this milestone.
- Independent review closed four findings: observe complete decoded batches
  before yielding; retain final error-frame usage before committing failure;
  recognize validated failed Responses JSON as terminal; and prevent rejected
  trailing Responses frames from replacing previously validated usage. Real
  HTTP/PostgreSQL cases cover client drop inside a Messages/Chat event, partial
  versus final counts, malformed failure identities and cross-response trailing
  data. Existing secret-redaction and provider-status assertions are retained.
- `/tmp/bitrouter-p1-dispatch-terminal-identity-tests.log`: **83 passed**, 2,179
  filtered out, zero retries, 9.719 seconds; real PostgreSQL foundation campaign
  9.269 seconds. `/tmp/bitrouter-p1-dispatch-local-final.log`: **99 passed**, 2,177
  filtered out, zero retries, 1.562 seconds with `--no-default-features`, including
  independent model invocation and gateway protocol boundary regressions.
- All-feature/all-target `cargo check --locked -p bitrouter` passed in
  `/tmp/bitrouter-p1-dispatch-all-targets.log`. Runtime helpers still awaiting
  serving integration emit unused-code warnings; this is not a final strict
  lint pass and no warning is suppressed.
- The initial enlarged PostgreSQL fixture overflowed its test stack. Independent
  scenarios are now separate boxed futures under the original stack and deadline;
  neither limit was raised. The initial pending-terminal failure and later helper
  name-shadowing compile error remain in their local logs. Final independent
  dispatch review approved the corrected source with no remaining findings.
- This is executor integration, not process-level acceptance. HTTP admission,
  immutable serving snapshots, durable execution finalization before delivery,
  bounded ownership/reconciliation, trajectory catch-up, operator/SDK/startup
  wiring and all P1-A01–A09 gates remain pending.

## Bounded custody and delivery accounting (2026-10-20)

- P1 is based on P0 `9cbe512`; the eight existing P1 patches rebased unchanged
  (`git range-diff` reported eight equivalent patches). The inherited P0 fix
  retains a prepared finalizer receipt until its consumer acknowledges custody,
  or rolls back with the original receipt when the consumer disappears.
  This candidate adds a nonqueueing local reservation before admission, retains
  a server-generated execution ID across cancellation and lost acknowledgements,
  and preserves the original admitted principal. Duplicate submissions never
  acquire custody of the existing execution.
- The registry retains each permit and content-free dispatch tracker until all
  request/provider/finalization owners have dropped. It never stores an admission
  or lends existing custody handles. A fair queue visits one entry at a time;
  a synchronous candidate guard requeues on cancellation without spawning I/O.
  Only verified terminal/unknown/resolved state or proven non-admission releases
  the local permit. Unknown shared tenant/provider reservations remain binding.
- An uncertain admission uses a two-statement READ COMMITTED probe: acquire its
  instance's exclusive fence after all local work ends, then read from a fresh
  snapshot. Confirmed admissions use an internal owner-checked read instead;
  normal completion does not acquire that exclusive instance fence. Revocation
  does not prevent the original owner from reconciling accepted work.
- Shared successful delivery now awaits authoritative execution finalization
  inside the existing delivery permit, after continuation activation and before
  the synchronous terminal-commit signal and return. Private custody follows
  detached finalizer preparation, authorization and compensation. Public
  finalization contracts remain unchanged.
- `/tmp/bitrouter-p1-custody-delivery-tests.log`: **74 passed**, 2,189 filtered,
  zero retries, 9.807 seconds; PostgreSQL campaign 9.471 seconds. This includes
  real pipeline/HTTP execution, blocked accounting, cancellation during delivery,
  detached preparation/rollback, duplicate and revoked-key retirement, cancelled
  queue candidates, lost admission acknowledgement and post-lock SQL snapshots.
- `/tmp/bitrouter-p1-custody-local-tests.log`: **65 passed**, 2,214 filtered,
  zero retries, 1.524 seconds, using the minimum local profile. The selected
  protocol/authority/finalization regressions do not represent a full suite.
  `/tmp/bitrouter-p1-custody-all-targets.log`: all-feature/all-target check passed,
  53.26 seconds. Unwired runtime code still emits warnings; no strict lint pass
  or full shared serving claim is made. The initial missing test import is
  retained in `/tmp/bitrouter-p1-custody-initial-tests.log` and was corrected.
- Independent review identified two cancellation/failure boundaries. The queued
  preparation regression now verifies that shared custody survives until the
  original receipt is compensated exactly once. A failure-bearing pipeline
  result also cannot reuse an earlier successful provider attempt as execution
  success: terminal validation uses the latest observed terminal attempt, and
  delivery and retirement share the same outcome decision.
- A frozen pipeline failure is recorded synchronously in bounded registry-owned
  custody before exposing the delivery permit or awaiting finalization. Dropping
  that permit unpolled or cancelling a blocked terminal transaction therefore
  retains the failure decision. This does not rewrite provider observations or
  reverse an already committed terminal/operator result. The local marker is not
  a claim that uncommitted decisions survive a hard process kill.
- `/tmp/bitrouter-p1-custody-outcome-before.log` reproduced the original
  failure-bearing result incorrectly committing success. The narrower
  `/tmp/bitrouter-p1-custody-cancelled-outcome-before.log` reproduced the remaining
  unpolled-permit failure before the custody-owned marker was implemented.
  `/tmp/bitrouter-p1-custody-cancelled-outcome-tests.log`: **117 passed**, 2,147
  filtered, zero retries, 13.213 seconds; PostgreSQL campaign 9.413 seconds.
  Cases preserve original attempt/usage facts, restore exact tenant/resource
  occupancy, retain one stable outbox event and verify idempotent retirement.
- An adjacent HTTP-rendering path was then reproduced in
  `/tmp/bitrouter-p1-custody-render-failure-before.log`: explicit delivery failure
  still retired a successful provider observation as execution success. The
  failure API now records its decision synchronously before returning its future,
  waits for required compensation, and awaits the authoritative failed terminal
  before signalling completion. Known preparation/activation, execution and
  stream failures likewise retain their decision before cancellable work.
  A bare disconnect remains distinct from an explicit failure.
- `/tmp/bitrouter-p1-custody-explicit-failure-final-tests.log`: **117 passed**,
  2,147 filtered, zero retries, 13.761 seconds; PostgreSQL campaign 10.054 seconds.
  Additional cases cover unpolled rendering-failure futures, blocked failed
  terminal commits, prepare/activation rejection with cancellation during
  compensation, direct execution errors and unsuccessful stream terminals.
  The local result check borrows the finish reason; it does not construct or
  hash another continuation context on the ordinary request path.
- Re-review found an earlier hop-observer cancellation window. The deterministic
  `/tmp/bitrouter-p1-custody-observer-before.log` regression failed at that exact
  signalled boundary. The selected failure-bearing generation result is now
  recorded before report/observer awaits, while ordinary retryable attempt errors
  remain eligible for successful fallback. The new scenarios also retain a
  successful-result cancellation control and a retryable-error/success control.
- Final C2-R3 shared selection, `/tmp/bitrouter-p1-custody-observer-tests.log`:
  **117 passed**, 2,147 filtered, zero retries, 13.571 seconds; PostgreSQL 9.925
  seconds. All-feature selection with the three protocol integration binaries,
  `/tmp/bitrouter-p1-custody-observer-full-tests.log`: **150 passed**, 3,421
  filtered, zero retries, 13.936 seconds; PostgreSQL 10.053 seconds.
  The same selected local regressions with `--no-default-features`,
  `/tmp/bitrouter-p1-custody-observer-local-tests.log`, passed **136/136**, 2,144
  filtered, zero retries, 4.891 seconds without a shared backend requirement.
  Rust 1.97 all-feature library/test Clippy completed with exit 0 in 2m32s
  (`/tmp/bitrouter-p1-custody-observer-clippy.log`), with only staged unused-code
  diagnostics and the existing dependency future-compatibility notice. No
  diagnostics were suppressed; this is not final strict P1 lint acceptance.
  Formatting and whitespace checks passed. The 13 Rust source hashes match the
  independent review; only the progress/specification records changed afterward.
- An earlier broad `--run-ignored all` selection unintentionally included two
  external-adapter opt-in tests without their required environment. Its log,
  `/tmp/bitrouter-p1-custody-all-features-tests.log`, contains 176 passes and two
  failures and is not an aggregate pass. Subsequent selections name only the
  intended shared/finalization tests and protocol integration binaries.
- Final independent source re-review approved the bounded custody/delivery
  substage, closing the receipt handoff, failure-bearing result, cancellation,
  explicit failure and hop-observer findings. This is scoped source acceptance;
  it does not close HTTP/runtime integration or aggregate acceptance.
  Design review requires
  transport-owned buffering to be bounded beyond response-body lifetime and
  heartbeats to be scheduled independently of slow reconciliation batches.
  These remain explicit serving/lifecycle obligations. Native continuation
  compensation has its own bounded owner and drain; an empty gateway registry
  alone does not prove that compensation has drained.
- The transport design review additionally found that early HTTP/1 responses
  can accumulate response headers across pipelined requests despite Hyper's
  body-buffer threshold. Shared transport must disable keep-alive or enforce a
  proved finite connection-reuse/flush bound, in addition to bounded outgoing
  chunks, metadata, connection/task counts and no-progress write deadlines.
- HTTP pre-extractor reservation, transport bounds, heartbeat/recovery tasks,
  startup, publication, operator/SDK APIs and trajectory integration remain
  pending. No P1-A01–A09 acceptance gate is closed by this substage.
