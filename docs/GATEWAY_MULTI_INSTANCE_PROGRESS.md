# Native gateway multi-instance progress

Implementation follows the [P1 contract](GATEWAY_MULTI_INSTANCE_SPEC.md) and the
[original P1 gates](BITROUTER_SERVICE_REFACTOR_PLAN.md#p1-acceptance).
Status: **in progress; no P1 delivery gate is accepted yet**.

Current P0 base (rechecked 2026-10-21): `9cbe512d101e777470d113b0086ab9e331b3a638`.
[#984](https://github.com/bitrouter/bitrouter/pull/984) is merge ready and remains
unmerged: [CI](https://github.com/bitrouter/bitrouter/actions/runs/39421381228)
passed 57/57 jobs, all checks were 64 successful/8 configured skips, and no
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
| 1 Authority foundation | Store/key foundation, shared startup and explicit stock bootstrap implemented | PostgreSQL foundation/runtime and blank-database stock bootstrap passed in minimal/full selections; local regressions and bounded lint passed | Foundation/runtime and bounded bootstrap backend/CLI/process reviews approved |
| 2 Admission and execution | Private engine and served admission/execution implemented; complete process acceptance pending | Real PostgreSQL transaction/race and scoped served execution tests passed | Private engine and serving substages approved |
| 3 Gateway integration | Official foreground startup, credential binding, admission, dispatch, delivery and HTTP router implemented; complete process acceptance pending | Scoped HTTP/protocol/executor and PostgreSQL runtime regressions plus official-process startup/idle-drain tests passed | Authentication/context/dispatch/custody/HTTP/runtime substages approved |
| 4 Published authority | Immutable publication, runtime and operator HTTP/SDK/CLI wiring implemented | PostgreSQL publication/handler, two-process operator and scoped feature regressions passed | Publication and bounded operator HTTP/SDK/CLI stages approved |
| 5 Trajectory authority | Pending | Pending | Pending |
| 6 Delivery and operations | Shared submission/query SDK, bounded custody/transport/HTTP, operator HTTP/SDK/CLI, retained runtime workers and durable attempt accounting facts and scoped metering storage implemented; projection receiver and outbox pending | Scoped runtime/service regressions, attempt accounting persistence and independent SDK tests passed | Custody/transport/HTTP/SDK/runtime and bounded operator HTTP/SDK/CLI substages approved; durable attempt evidence approved; bounded metering review approved with recorded validation limitations |
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

## Shared transport substage (2026-10-20)

- Added a shared-only HTTP/1 transport helper under the `multi-instance` feature.
  Local serving remains on its existing Axum path. The new helper is not yet
  attached to the official shared runtime; this stage closes neither serving
  integration nor any P1-A01–A09 gate.
- The earlier design finding about accumulated response heads is addressed by
  permitting one request per connection and disabling upgrades/HTTP/2. This
  deliberately gives up connection reuse in the initial shared transport.
  Connection setup cost and independent-process load remain acceptance work.
  Connection permits persist beyond response-body EOF. Both live connections
  and unreaped owned task records are capped; overload creates no waiting task.
  Shutdown closes the listener, then drains or aborts and joins connection tasks.
- Outgoing DATA is copied into independently owned chunks of at most 64 KiB,
  including narrow slices of large backing allocations. The wrapper retains
  at most one original source frame/remainder and preserves body hints, EOS and
  trailers. Metadata counts duplicates and bounds serialized size with framing
  overhead reserved. These limits are not an exact connection/RSS budget.
- Added independent header, ingress and output-stall deadlines, plus a connection
  cap, to validated process configuration and the operator skill reference.
  Generated configuration schema now includes the shared-mode contract.
  Oversized known request lengths are rejected before dispatch; body allocation
  hints cannot exceed the remaining byte allowance. Ingress completion ends its
  deadline, while legitimate inference has no added whole-request timeout.
- Real pinned-Hyper regressions cover final JSON/SSE frame consumption before a
  blocked flush, pipelined early responses, scalar/vectored writes, incomplete
  ingress, overload and owned shutdown. Unit tests cover source allocation
  release, pending remainder/EOS/hints, trailers and no-progress timing.
- The first run (`/tmp/bitrouter-p1-transport-first.log`) had 11 passes and one
  failing allocation-lifetime assertion. The fixture retained an independent
  original `Bytes` reference beside its narrow slice; replacing that reference
  corrected the fixture. The subsequent shared selection passed 129 tests.
- Initial independent review confirmed the single-request limit closes the
  response-head accumulation finding, but found premature transport shutdown.
  `/tmp/bitrouter-p1-transport-shutdown-before.log` reproduced all three cases:
  shutdown skipped a pending write; real Hyper released a blocked graceful
  response; and an early response closed before its write deadline.
- Pending writes/flushes now form a shutdown barrier. Their timers remain
  registered while Hyper retries its own buffered flush. A separate underlying
  shutdown timer prevents a no-op flush from restarting that deadline.
  Scalar/vectored real-Hyper regressions verify complete response bytes when
  reading resumes, plus retained capacity until timeout when it does not.
  Additional tests cover pending flush and underlying shutdown timing.
- `/tmp/bitrouter-p1-transport-shutdown-fixed-tests.log`, Rust 1.99,
  `--locked -p bitrouter --no-default-features --features multi-instance --lib`,
  explicit shared/executor/continuation/config selection, `--run-ignored all`,
  two test threads and zero retries: **135 passed**, 2,146 filtered, 13.721
  seconds; real PostgreSQL fixture 10.151 seconds.
- Final source under `--all-features` with the same library selection and the
  `model_invocation`, `gateway_model_boundary` and `gateway_anthropic_authority`
  integration binaries passed **168/168**, 3,420 filtered, zero retries in
  13.933 seconds; PostgreSQL 10.068 seconds
  (`/tmp/bitrouter-p1-transport-shutdown-full-tests.log`). The existing Apple
  development-linker `__eh_frame` size warning also appeared in the prior
  custody-stage run; this is not a release-performance measurement.
  Pure local `--no-default-features` selection passed **137/137**, 2,143
  filtered, zero retries in 4.914 seconds without a PostgreSQL test URL
  (`/tmp/bitrouter-p1-transport-local-tests.log`).
- Final Rust 1.97 all-feature library/test Clippy completed with exit 0 in
  1m01s (`/tmp/bitrouter-p1-transport-final-clippy.log`). It reported no
  Clippy-specific findings, but still reports 88 staged unused-code diagnostics
  for the not-yet-wired shared runtime, one library-test unused item, and the
  existing dependency future-compatibility notice. None were suppressed; this
  is not final strict P1 lint acceptance.
- Independent re-review approved the transport substage and closed both the
  response-head accumulation and premature-shutdown findings. The 16 reviewed
  repository-file hashes match the final source/schema/reference files
  (`/tmp/bitrouter-p1-transport-review-hashes.json`); only progress evidence is
  updated afterward. The frozen review is
  `/tmp/bitrouter-p1-transport-stage-review-20261020.json`.
  Formatting and whitespace checks passed. Full shared serving, dedicated
  heartbeat/recovery workers, operator/SDK surfaces, trajectory integration,
  independent-process faults/load, final performance and aggregate review
  remain open.

## Shared HTTP admission substage (2026-10-20)

- Added a private shared HTTP router for the four inference protocols, execution
  queries, models, health and metrics. It uses the existing model parser,
  transforms, pipeline and renderers. It is not yet wired to official process
  startup; this substage does not establish independent-process acceptance.
- Nonqueueing custody reservation precedes JSON aggregation. One immutable
  application/generation pair supplies both admission and execution. First
  submissions retain JSON/SSE responses, duplicates return 202 with the original
  execution, and conflicting key reuse returns 409. Execution queries and
  duplicates require fresh database-backed authentication.
- Caller correlation is separate from the service ID installed before pipeline
  hooks, provider dispatch and continuation encoding. Accepted responses,
  including errors, carry execution/query metadata and hold custody through
  response-body ownership. Unconfirmed admission errors expose a candidate ID;
  a subsequent 404 can establish that it did not commit. No response causes
  automatic inference replay.
- The request fingerprint is computed from the original JSON and a fixed set of
  23 semantic headers before mutable transforms or route selection. It preserves
  duplicate/raw header values and distinguishes absent from empty headers.
  Resolved provider passthrough outside that set is rejected at publication.
  Shared session context retains session evidence without local ACP route leases.
  The encoding and an independently calculated golden vector are in the spec.
- Independent review found a missing managed Responses profile guard. Before
  the fix, `/tmp/bitrouter-p1-http-managed-before.log` reproduced a beta-header
  retry incorrectly returning the ordinary execution with 202. Shared ingress
  now reuses managed-profile detection and the unsupported-capability response,
  inside the reserved path and before admission/duplicate lookup. Regression
  coverage checks beta variants, body markers, allowed ordinary requests,
  unchanged durable record counts/tenant state and no provider dispatch.
- The first HTTP fixture incorrectly scoped its auth hook to Generation.
  Changing it to the official Both/pre-resolution scope fixed Decisions.
  A later streaming case overflowed the cumulative debug stack of the enclosing
  PostgreSQL campaign. LLDB captured that call chain in
  `/tmp/bitrouter-p1-http-stack-backtrace.log`; boxing the common execution future
  did not fix it and was reverted. Running HTTP scenarios in one owned, joined
  task, like real connections, passed with the ordinary stack budget.
  `/tmp/bitrouter-p1-http-owned-task.log` also exercised real TCP JSON and SSE.
  No stack-size override or test deadline increase was used.
- Additional regressions cover a 256-byte caller correlation with a continuation
  handle that decodes to the service execution ID; body abandonment with observed
  terminal/usage versus genuinely unknown work; retained shared occupancy for
  unknown work; tenant-scoped status and revoked credentials; and a real
  database admission timeout exposing a candidate ID without automatic replay.
  The initial abandonment assertion incorrectly expected a terminal before the
  provider terminal was read; the fixture now explicitly distinguishes those
  two outcomes. Post-commit lost-ACK injection and real publication/snapshot
  replacement during serving remain aggregate/runtime validation work.
- `/tmp/bitrouter-p1-http-boundaries-tests.log`, Rust 1.99,
  `--locked -p bitrouter --no-default-features --features multi-instance --lib`,
  explicit shared/HTTP/executor/continuation/config selection, `--run-ignored all`,
  two test threads and zero retries: **175/175 passed**, 2,110 filtered in
  17.647 seconds; PostgreSQL 13.631 seconds.
- `/tmp/bitrouter-p1-http-full-tests.log`, `--all-features` with the same library
  selection plus `model_invocation`, `gateway_model_boundary` and
  `gateway_anthropic_authority`: **208/208 passed**, 3,384 filtered, zero retries
  in 17.909 seconds; PostgreSQL 13.636 seconds. The existing Apple development
  linker warning appeared again; this was not a performance run.
- `/tmp/bitrouter-p1-http-local-tests.log`, pure local `--no-default-features`
  without a PostgreSQL test URL, HTTP/execution/continuation/config selection:
  **146/146 passed**, 2,107 filtered, zero retries in 3.967 seconds.
- Clippy identified one unnecessary comparator; the equivalent key-based sort
  preserves the fingerprint. The final fingerprint/golden selection passed
  **3/3**, zero retries, 0.035 seconds
  (`/tmp/bitrouter-p1-http-final-fingerprint.log`). Independent review verified
  that this was the only source delta after the broader successful runs.
- Final Rust 1.97 all-feature library/test Clippy completed with exit 0 in
  1m01s (`/tmp/bitrouter-p1-http-final-clippy.log`) and no Clippy-specific
  diagnostics. It still reports 122 staged unused-code diagnostics in the
  library and one in the library tests, plus the existing dependency
  future-compatibility notice. These remain runtime-integration work and were
  not suppressed; this is not final strict P1 lint acceptance.
- Independent rereview approved the HTTP substage, closed the profile finding,
  and verified the sort equivalence. All 16 reviewed source/spec file hashes
  match the final candidate
  (`/tmp/bitrouter-p1-http-stage-review-20261020.json`,
  `/tmp/bitrouter-p1-http-review-hashes.json`). Formatting and whitespace checks
  passed. The progress ledger alone was updated after the reviewed snapshot.
- Runtime publication/drain ownership, dedicated
  heartbeat/recovery workers, operator/SDK clients, trajectory integration,
  independent-process faults/load, performance and final aggregate review remain
  open. No P1-A01–A09 gate is closed here.

## Shared gateway SDK substage (2026-10-20)

- Added `gateway::execution::{SubmissionOptions, ExecutionMetadata, Submission}`
  and explicit shared-gateway submission methods: four JSON protocols, three
  streaming protocols, plus `GatewayClient::execution(id)`. No new crate,
  dependency or SDK feature is required. `bitrouter-ai` remains unchanged.
- HTTP 200 returns ordinary output/stream with its identity; 202 returns the
  original execution projection before any model or SSE decoding. The existing
  model methods remain local-compatible and explicitly reject 202 while keeping
  its HTTP evidence. The existing `Error` enum and failure fields are unchanged.
- New calls preserve HTTP status and headers on malformed, oversized or
  truncated 200/202 responses using `Error::Service` with `body_error`. Status
  locations are validated against the configured origin/prefix and execution
  ID, but never used to choose a credential destination. Explicit queries encode
  IDs as one path segment. Duplicate response correlation can differ from the
  original correlation in the execution DTO.
- Options are validated on every call against the fixed 23-header contract.
  Duplicate order, empty values and typed identity normalization are preserved.
  New submissions have a stable default User-Agent; legacy calls retain their
  versioned default. Managed-profile markers and authority/framing overrides
  are rejected before sending. New Debug implementations redact options,
  metadata and generic outputs without requiring payloads to implement Debug.
- The SDK never retries, follows status Location, generates idempotency keys,
  polls or synthesizes model results. A shared submission sent to a local
  gateway can execute before missing metadata is detected; README/rustdoc and
  the specification document this limitation and explicit recovery semantics.
- Independent design review identified status classification, evidence loss,
  duplicate correlation, proxy-prefix routing, header identity, local-mode
  semantics, source compatibility and diagnostic-redaction requirements.
  The implementation follows those requirements. The existing-execution DTO
  is boxed to bound the enum variant size.
- After correcting integration-module file paths, the first complete SDK
  `--all-features` suite passed **72/72**, zero skips/retries in 0.472 seconds
  (`/tmp/bitrouter-p1-sdk-http-tests.log`). Added Location, truncated-success
  and legacy/User-Agent regressions raised this to **75/75**, zero skips/retries
  in 0.497 seconds (`/tmp/bitrouter-p1-sdk-boundaries-tests.log`).
- `--no-default-features` passed **61/61**, zero skips/retries in 0.398 seconds
  (`/tmp/bitrouter-p1-sdk-minimal-tests.log`). Rustdoc compiled **6/6** examples
  in each profile (`/tmp/bitrouter-p1-sdk-doc-tests.log` and
  `/tmp/bitrouter-p1-sdk-minimal-doc-tests.log`). These fixtures import only the
  SDK/public contracts and run isolated HTTP servers, not application assembly.
- Strict Rust 1.97 SDK `--all-features --all-targets` Clippy with `-D warnings`
  passed in 11.34 seconds (`/tmp/bitrouter-p1-sdk-clippy.log`). Formatting and
  whitespace checks passed. The forward production dependency tree contains
  no service/database/UI crate, and the SDK's 23 header names/order exactly
  match the service contract (`/tmp/bitrouter-p1-sdk-dependencies.txt`).
- Independent implementation review found three validation defects: flattened
  `extra.stream` could override the typed mode, a duplicate's original DTO
  correlation was not independently validated, and a comma-coalesced Content-Type
  could hide after a parameter. Regression tests reproduced all three before
  production repairs, with two positive controls passing
  (`/tmp/bitrouter-p1-sdk-review-red.log`). The repaired methods reject reserved
  stream extensions before HTTP, validate both correlations independently, and
  parse complete textual media types including quoted escapes/commas and empty
  parameters without adding dependencies.
- After repairs, the SDK all-feature suite passed **81/81**, zero skips/retries
  in 0.247 seconds (`/tmp/bitrouter-p1-sdk-review-green.log`), and the minimal
  profile passed **67/67**, zero skips/retries in 0.235 seconds
  (`/tmp/bitrouter-p1-sdk-review-minimal.log`). Strict Rust 1.97 all-feature,
  all-target Clippy passed in 1.04 seconds
  (`/tmp/bitrouter-p1-sdk-review-clippy.log`).
- Both final Rustdoc profiles passed **6/6** examples
  (`/tmp/bitrouter-p1-sdk-review-doc.log`,
  `/tmp/bitrouter-p1-sdk-review-minimal-doc.log`); formatting and whitespace
  checks passed. Independent rereview approved the substage and closed all
  three findings (`/tmp/bitrouter-p1-sdk-repair-review.json`). All 31 reviewed
  file hashes match (`/tmp/bitrouter-p1-sdk-review-hashes.json`).
- This is shared
  inference/status client delivery, not the operator API or official process
  runtime. Publication/drain workers, trajectory integration, independent
  process acceptance, performance and final aggregate review remain open.

## Dedicated heartbeat prerequisite (2026-10-20)

- A renewal-only capability now derives from the original joined engine and
  substitutes a dedicated PostgreSQL pool while preserving owner, namespace,
  deployment key, lease and transaction deadline. It neither initializes
  authority nor joins another incarnation. Renewal holds the shared authority
  fence and original owner lock, preserves drain state, and returns database
  time/expiry/acceptance evidence only after commit. Definite expiry remains a
  typed rejection; other failures can include an uncertain acknowledgement.
- Process configuration now requires a total connection budget of **2–128**;
  the intended split is one heartbeat connection and `N-1` main connections.
  The low-level connector still permits a one-connection pool. Configuration
  and the shipped skill reference agree on these bounds. Official pool creation
  and the supervisor are still pending; this primitive does not enforce the
  caller's pool selection or prove a running process respects the total budget.
- The real PostgreSQL regression uses two one-connection pools. It occupies the
  main pool during renewal, checks the persisted expiry/revision, demonstrates
  that an owner-row lock still times out renewal, recovers after releasing that
  lock, renews while draining, forces database-clock expiry, and verifies that
  repeated renewals reject without creating a replacement owner.
- Minimal shared-feature focused validation passed **2/2**, 2,284 filtered,
  zero retries in 0.588 seconds; the PostgreSQL case took 0.577 seconds
  (`/tmp/bitrouter-p1-lease-tests.log`). All-feature shared/config regressions
  passed **31/31**, 3,535 filtered, zero retries in 13.648 seconds, including
  the complete existing PostgreSQL campaign in 13.564 seconds
  (`/tmp/bitrouter-p1-lease-regressions.log`).
- Pure local `--no-default-features` configuration regression passed **1/1**,
  2,252 filtered, zero retries in 0.025 seconds without a PostgreSQL test URL
  (`/tmp/bitrouter-p1-lease-local.log`). Rust 1.97 all-feature library/test Clippy
  exited successfully in 1m01s with no Clippy-specific diagnostics
  (`/tmp/bitrouter-p1-lease-clippy.log`). It retains 126 library and one
  library-test unused-code warnings from runtime integration that is still
  staged; no warning was suppressed and this is not final strict P1 lint
  acceptance. Formatting and whitespace checks passed.
- Independent review found no actionable defects in the six implementation/
  skill files (`/tmp/bitrouter-p1-lease-review.json`). Lost commit-ACK fault
  injection, official pool allocation and supervised readiness remain later
  runtime/process validation, not evidence supplied by this focused test.
- The runtime design audit additionally identified complete config/key revision
  fencing, atomic generation custody, cancellation-safe task/finalizer drains,
  finite retained generations and process-owned observability as required
  integration work (`/tmp/bitrouter-p1-runtime-design-review.json`). No P1 gate
  is closed by this prerequisite.

## Snapshot revision and request custody (2026-10-20)

- Shared snapshots now carry the complete configuration/key revision and a
  distinct process-local resource identity. Publication permits key-only
  advancement and rejects a regression of either component. New admission
  checks both components under the authoritative head fence. Freshly
  authenticated duplicates still observe their original execution without
  acquiring new work; key rotation alone does not cancel accepted provider work.
- Snapshot selection and pre-body custody registration occur under the same
  short publication gate. Per-generation counts include unstarted, uncertain
  and accepted reservations until successful reconciliation. Cancelled
  reconciliation retains both its queue entry and generation count. Counts
  disappear only when the last owned entry retires.
- Models and metrics handlers retain their snapshot across awaited work.
  `retired_quiescent` checks that a snapshot is no longer current, has no
  request custody, and has no other snapshot readers. This is only a local
  request/reader barrier; it does not authorize skipping owned task joins,
  continuation compensation, required finalizers or exporter shutdown.
- Real PostgreSQL/HTTP regressions rotate keys while an old request body is
  still pending and while a live response stream retains its old application.
  The former fails admission without provider dispatch; the latter retains
  resources until cancellation/reconciliation and remains truthfully Unknown.
  Snapshot readers independently prevent quiescence. Additional cases reject
  stale/future key admission without quota consumption, preserve duplicate
  identity and accepted work, and reject mixed config/key regressions.
- The first new mixed-regression fixture subtracted a configuration generation
  of one to zero, reaching constructor validation before the intended
  publication check. The fixture now first installs a valid later configuration
  and tests a nonzero regression against it. No production behavior changed
  for this fixture correction.
- Minimal shared-feature shared/config validation passed **31/31**, 2,255
  filtered, zero retries in 14.099 seconds; the PostgreSQL campaign took
  14.027 seconds (`/tmp/bitrouter-p1-generation-tests.log`).
- All-feature shared/config validation passed **31/31**, 3,535 filtered, zero
  retries in 13.698 seconds; the PostgreSQL campaign took 13.616 seconds
  (`/tmp/bitrouter-p1-generation-full-tests.log`). Rust 1.97 all-feature
  library/test Clippy completed with exit 0 in 1m01s and no Clippy-specific
  diagnostics (`/tmp/bitrouter-p1-generation-clippy.log`). It reports 130
  library and one library-test staged unused-code warnings; no suppression was
  introduced. Formatting and whitespace checks passed. Final strict P1 lint
  acceptance still requires official runtime integration.
- Independent review found no actionable production correctness defects
  (`/tmp/bitrouter-p1-generation-review.json`). Its documentation finding is
  addressed: configuration-generation changes invalidate pending dispatch;
  key-only rotation preserves accepted work. The local quiescence predicate is
  not a sealed producer gate. Explicit producer tickets remain necessary for
  ordered owned-drain phases. Official startup, head/load supervision and
  readiness, finite retained generations,
  owned task/finalizer drains and trajectory epoch binding remain open. No P1
  acceptance gate is closed by these internal lifecycle boundaries.

## Owned pipeline tasks and producer tickets (2026-10-21)

- Shared assembly now selects persistent execution/settlement task ownership.
  Local builds retain the existing task tracker and settlement drain behavior.
  Shared pipelines explicitly reject the local error-returning shutdown entry
  rather than silently draining empty local collections.
- HTTP snapshot selection acquires a non-clonable producer ticket under the
  publication gate. Publication closes the old gate, while already acquired
  tickets remain valid through body destruction. JSON/SSE EOF alone does not
  release a ticket. Models/metrics readers also retain tickets. Binding one
  pipeline to more than one serving snapshot is rejected.
- A cancelled/expired drain retains its task handles and evidence. Reaping is
  bounded per poll and a single waiter owns the join wakeup. Task and panic
  payload destruction can register descendants without holding the owner lock;
  the empty fixed point is checked after those destructors run. Closing sources
  does not reject descendants of previously accepted work. Completing the
  fixed point seals registration; late work is discarded without polling and
  recorded as a lifecycle failure.
- Force requires all external producer tickets to be released. Existing tasks
  are aborted and retained for joining; children registered after force are
  destroyed by owned wrappers without polling their asynchronous work. Forced,
  cancelled, panicked, discarded, late-registration and required-finalizer
  failure outcomes remain distinct. Rollback failure is retained even if its
  containing task returns successfully.
- Initial minimal shared/config and new lifecycle validation passed **41/41**,
  2,255 filtered, zero retries in 13.555 seconds; the PostgreSQL campaign took
  13.442 seconds (`/tmp/bitrouter-p1-owned-tasks-tests.log`). Subsequent additions
  cover sealing, rollback-failure accounting and retained JSON/SSE EOF bodies;
  final candidate minimal shared validation passed **43/43**, 2,255 filtered,
  zero retries in 15.677 seconds, including the PostgreSQL campaign in 15.561
  seconds (`/tmp/bitrouter-p1-owned-tasks-final-minimal.log`).
- Final all-feature pipeline/shared/config regressions passed **412/412**,
  3,166 filtered, zero retries in 15.702 seconds; PostgreSQL took 13.641 seconds
  (`/tmp/bitrouter-p1-owned-tasks-full-tests.log`). Pure local
  `--no-default-features` pipeline/config regressions passed **255/255**, 1,998
  filtered, zero retries in 1.156 seconds
  (`/tmp/bitrouter-p1-owned-tasks-local-tests.log`).
- Rust 1.97 all-feature library/test Clippy exited 0 in 1m01s with no
  Clippy-specific diagnostics (`/tmp/bitrouter-p1-owned-tasks-clippy.log`).
  Its 138 library and one library-test unused-code warnings remain staged
  runtime integration work; no suppression was introduced. This is not final
  strict P1 lint acceptance. Formatting and whitespace checks passed.
  Independent implementation review approved this substage with no actionable
  findings (`/tmp/bitrouter-p1-owned-tasks-review.json`); all 15 candidate files
  and four validation logs matched the reviewed fingerprints.
- This substage does not implement the complete generation resource owner,
  bounded retained generations, continuation worker/marker drain, startup
  supervisor or process shutdown. A joined pipeline is not sufficient to
  release its application resources. All P1 acceptance gates remain open.

## Owned continuation-worker drain (2026-10-21)

- Shared assembly selects the continuation lifecycle before sharing its
  registry. Local worker startup/drain behavior remains unchanged. The shared
  worker can start once; withdrawal or failure cannot silently create a new
  worker. Readiness supervision can distinguish a healthy lazy/running worker
  from a finished worker or a withdrawn registry.
- The original join handle stays in its owner until completion is observed.
  Cancelled waits preserve that handle, its irreversible restart gate and
  pending marker map. One active waiter performs joins. Panic payloads are
  destroyed outside the control lock after the restart gate closes.
- Graceful drain applies the caller's single absolute deadline across worker
  joining, asynchronous identity locks and every database reconciliation
  batch. An expired retry starts no additional reconciliation; a deliberately
  later attempt can retry on the same retained owner. Backend failures and
  no-progress outcomes retain their markers and stopped-worker evidence.
- All marker phases count, including `Reserved`. Force aborts and joins the
  original worker without starting database compensation; unresolved markers
  remain recorded. Pipeline producer/task barriers must precede this drain.
- Initial focused lifecycle tests passed **6/6**, 2,298 filtered, zero retries
  in 0.620 seconds (`/tmp/bitrouter-p1-continuation-owned-tests.log`). They use
  existing isolated SQLite fixtures plus controlled worker/identity-lock waits
  to test cancellation, force, panic, multi-batch deadlines and a cancelled
  bind retaining a `Reserved` marker. A seventh backend-failure/retry case was
  then added.
- Final all-feature continuation/pipeline/shared/config regression passed
  **518/518**, 3,067 filtered, zero retries in 19.790 seconds; the PostgreSQL
  campaign took 13.705 seconds
  (`/tmp/bitrouter-p1-continuation-owned-full-tests.log`).
  Pure local `--no-default-features` continuation regression passed **93/93**,
  2,160 filtered, zero retries in 3.766 seconds
  (`/tmp/bitrouter-p1-continuation-owned-local-tests.log`). Minimal shared
  continuation regression passed **100/100**, 2,205 filtered, zero retries in
  4.184 seconds (`/tmp/bitrouter-p1-continuation-owned-minimal-tests.log`).
- Rust 1.97 all-feature library/test Clippy exited 0 in 1m02s with no
  Clippy-specific diagnostics (`/tmp/bitrouter-p1-continuation-owned-clippy.log`).
  The 148 library and one library-test unused-code warnings remain staged
  runtime integration work; no suppression was introduced. This is not final
  strict P1 lint acceptance. Formatting and whitespace checks passed.
- Independent review approved the narrow substage with no actionable findings
  (`/tmp/bitrouter-p1-continuation-owned-review.json`). The future resource
  owner must retain the original registry and attempt deadline, join pipeline
  producers first, and preserve panic/forced/error evidence separately from
  marker counts. `retained == None` or an empty marker map alone is insufficient
  to claim a clean drain.
- This stage does not migrate continuation lease decisions to the database
  clock, establish independent-process fencing, or implement complete
  generation resource ownership and the process supervisor. The fixture
  results are not PostgreSQL concurrency/process acceptance. All P1 gates
  remain open.

## Shared native transaction authority and bind/renew (2026-10-21)

- An initial PostgreSQL regression reproduced the legacy continuation renewal
  gap: a valid authenticated row with an expired lease was renewed by its old
  owner (`/tmp/bitrouter-p1-continuation-clock-red.log`, one failing test).
  The new shared backend is explicit; backend type, Cargo feature selection and
  owned worker lifecycle do not change local caller-clock semantics.
- Shared producer records now fence binds with `FOR SHARE`; a committed
  `FOR UPDATE` close waits for already admitted writes and rejects delayed
  writers. Closure evidence is bound to one namespace and random producer.
  Cancelled/unconfirmed results are retained until a fresh primary read proves
  closure. Post-close inspection uses a fresh statement snapshot after its
  identity-lock wait. No production call joins Rust tasks inside a transaction.
- The shared migration ledger now owns a namespace-scoped native relation.
  Private bind/renew operations use the initialized namespace key authority,
  framed owner/request identities and namespace-bound authenticated encryption.
  They never bootstrap the local singleton epoch. Local crypto helpers were
  factored without changing their authenticated data or payload interpretation.
- The native row is locked before the database clock is sampled. Renewing an
  expired or superseded token fails; a cleanup token cannot renew forward
  authority. The final SQL mutation checks old lease, retention and replacement
  lease again after sealing. Renewal preserves retention and reauthenticates
  metadata. A cached future lease cannot bypass the row lookup.
- Initial gate validation passed **3/3**, then initial gate/native tests passed
  **7/7** (`/tmp/bitrouter-p1-continuation-gate-tests.log`,
  `/tmp/bitrouter-p1-continuation-clock-tests.log`). Subsequent additions cover
  namespace-bound AEAD, metadata tampering, cached stale tokens, caller loss of
  a committed close result and post-lock snapshot freshness. The caller-loss
  fixture does not simulate a PostgreSQL wire disconnect.
- Final repaired minimal `multi-instance` continuation/shared regression passed
  **141/141**, 2,175 filtered, zero retries in 24.956 seconds
  (`/tmp/bitrouter-p1-continuation-clock-bounded-minimal.log`). Full-feature
  continuation/pipeline/shared/config regression passed **528/528**, 3,068
  filtered, zero retries in 29.297 seconds; PostgreSQL foundation took 13.732
  seconds (`/tmp/bitrouter-p1-continuation-clock-bounded-full.log`). Pure local
  `--no-default-features` continuation regression passed **93/93**, 2,160
  filtered, zero retries in 7.217 seconds
  (`/tmp/bitrouter-p1-continuation-clock-local.log`).
- Rust 1.97 all-feature library/test Clippy exited 0 in 1m02s with no
  Clippy-specific diagnostics
  (`/tmp/bitrouter-p1-continuation-clock-bounded-clippy.log`).
  The 163 library and one library-test unused-code warnings remain staged
  runtime integration work; no suppression was introduced. This is not final
  strict P1 lint acceptance. Formatting and whitespace checks passed.
- Independent native review found a legacy NUL-delimited target fingerprint
  collision at the new shared boundary. The shared implementation now frames
  the namespace/provider/account/protocol/endpoint/auth-scheme/credential-proof
  tuple under a new domain. A regression rejects the colliding provider/account
  tuple, preserves the first row, and still accepts an exact duplicate. Local
  persisted fingerprints remain unchanged. The final repaired test profiles
  above include this regression; local source was unchanged by the repair.
- Final boundary inspection identified a second issue: native payload size was
  checked after encryption under the database locks. Raw native IDs and the
  complete serialized plaintext are now capped before database access, leaving
  room for the authentication tag, base64 expansion and bounded row metadata.
  A closed-producer fixture distinguishes oversized raw IDs, serialization
  expansion and a valid-size control by their specific errors. The final
  profiles above include this repair.
- Independent reviews approved the transaction gate and the narrow native
  bind/renew implementation with no open findings
  (`/tmp/bitrouter-p1-continuation-gate-review.json`,
  `/tmp/bitrouter-p1-native-bind-renew-review.json`). Both target framing and
  pre-transaction size-bound findings are closed. All 11 candidate file and
  four final validation-log fingerprints were verified; runtime integration
  and P1 acceptance remain excluded from these approvals.
- Registry/assembly integration, forward state transitions, active resolution,
  pruning, expired cleanup claims and uncertain `Reserved` recovery are still
  open. The new backend currently implements private bind/renew only. These
  fixtures are not official-process acceptance; all P1 gates remain open.

## Shared native publication and retention (2026-10-21)

- Private forward transitions now enforce provisional-to-delivering-to-active
  order and exact producer/generation/lease authority at each database
  transaction. Delivery waits remain outside those transactions. Closing the
  producer rejects forward publication while allowing retained active lookups.
- Active resolution uses database retention time independently of the old
  publication lease. Resolved targets use the shared namespace-framed
  fingerprint; local persisted target matching remains unchanged.
- Expiry replaces native content with authenticated empty plaintext in an
  `expired` row, preserving original publication and retention metadata.
  Physical purge has its own database-time condition. Scans process at most
  32 records per total deadline and advance their cursor past errors. Mutations
  reacquire locks and compare full body plus revision, protecting a re-created
  identity even when both incarnations have revision zero.
- Initial minimal continuation/shared tests passed **146/146**, zero retries,
  in 19.242 seconds (`/tmp/bitrouter-p1-native-lifecycle-minimal.log`).
  A subsequent adversarial payload test then failed as intended: authenticated
  empty plaintext in an active shared row entered the legacy raw-ID fallback
  (`/tmp/bitrouter-p1-native-lifecycle-payload-red.log`). Shared decryption now
  rejects empty live payloads and nonempty expired payloads before any caller
  can interpret them. Local decoding is unchanged.
- Independent review identified `NL-I01`: advancing the cursor for a whole
  scanned batch could repeatedly skip its unattempted suffix after a lock
  timeout or cancellation. A PostgreSQL regression observes the actual blocked
  query before cancellation and fails against the original cursor behavior
  (`/tmp/bitrouter-p1-native-lifecycle-cursor-red.log`). The repaired cursor
  advances immediately before each attempted row and wraps only after the
  suffix is exhausted. The regression covers cancellation with two due rows
  and a total-deadline expiry with 32; it keeps the first lock held until the
  later rows are scrubbed, then verifies the first is retried after wraparound.
  The pre-fix failing run reaches the cancellation case first; it does not
  separately establish a pre-fix failure of the deadline case.
- The final repaired minimal continuation/shared regression passed **148/148**,
  2,175 filtered, zero retries in 21.309 seconds
  (`/tmp/bitrouter-p1-native-lifecycle-compact-minimal.log`). Final full-feature
  continuation/pipeline/shared/config regression passed **535/535**, 3,068
  filtered, zero retries in 23.590 seconds
  (`/tmp/bitrouter-p1-native-lifecycle-compact-full.log`). The real-lock
  cancellation/deadline regression passed both cases. Darwin's linker warned
  that this full-feature test image exceeded its compact-unwind section limit;
  these tests do not establish production artifact size or unwind performance.
- Pure local continuation regression passed **93/93**, 2,160 filtered, zero
  retries in 3.771 seconds
  (`/tmp/bitrouter-p1-native-lifecycle-compact-local.log`). All three final
  profiles include the subsequent compact-string adjustment.
- Clippy identified that adding namespace storage made the resolution enum too
  large. The private immutable fingerprint and optional namespace now use
  boxed strings, preserving the public enum and local matching behavior. Final
  Rust 1.97 all-feature library/test Clippy exited 0 in 1m02s with no
  Clippy-specific diagnostics
  (`/tmp/bitrouter-p1-native-lifecycle-compact-clippy.log`). The 167 library
  and one library-test unused-code warnings remain staged integration work;
  no suppression was added. This is not final strict P1 lint acceptance.
- Independent source review approved the cursor and compact-storage repairs
  with no open source findings
  (`/tmp/bitrouter-p1-native-lifecycle-review.json`). The candidate manifest
  records eight source/document fingerprints and four final validation logs
  (`/tmp/bitrouter-p1-native-lifecycle-candidate.json`). Formatting and
  whitespace checks passed. Registry/assembly integration, cleanup-only
  recovery, uncertain `Reserved` resolution and official-process acceptance
  remain open. No P1 acceptance gate is closed by this substage.

## Shared native cleanup and Registry integration (2026-10-21)

- Shared cleanup is durable and namespace-scoped. Receipt-matched cancellation
  can atomically demote an owned active generation to authenticated cleanup;
  retirement alone cannot. Cleanup ownership ignores obsolete observed leases
  while retaining the exact identity, generation and producer. Finishing compares
  body plus revision, including against a replacement at the same revision.
  Recovery scans at most 32 records, advances immediately before each attempt
  and uses a total database deadline. A peer can finish an acknowledged cleanup
  whose result was discarded by its caller; that fixture is not a wire-fault test.
- Delivery permits retain a sender-side terminal outcome. Successful accounting
  and terminal send precede the positive outcome in the same poll. A failure
  signal, dropped permit or unpolled failure future records negative evidence.
  Cancelling the receiver does not consume this evidence. Existing local
  handshake behavior remains intact.
- `ContinuationRegistry::new_shared` selects the initialized namespace backend.
  Bind, prepare, commit, rollback, lookup, prune and reconciliation now dispatch
  explicitly; the shared path does not use the local singleton key epoch or
  caller-provided clock. Shared assembly injects this capability, deriving both
  database and continuation key from it.
- The shared registry stores separate reserved, owned and non-owning existing
  preparations. It retains immutable intent and receipt identity through
  cancellation, invalidates an observed lease before renewal, and never promotes
  cleanup or retiring state back into forward publication. Eligible commits
  without preparation fail. Existing active duplicates cannot acquire cleanup
  ownership of the earlier binding.
- Owned drain closes admission to new native binds, joins the worker, then
  confirms the durable producer-close barrier before resolving reserved absence
  or foreign rows. The same absolute deadline covers that barrier and all marker
  work, including empty-map shutdown. Repeating an expired drain cannot issue
  producer-close SQL. Force only aborts/joins the worker.
- Initial minimal native/terminal selection passed **37/37**, zero retries in
  4.028 seconds (`/tmp/bitrouter-p1-native-registry-initial.log`). After adding
  expired-drain and uncertain-live-prepublication coverage, the minimal
  shared/continuation/terminal/assembly selection passed **174/174**, 2,165
  filtered, zero retries in 15.495 seconds
  (`/tmp/bitrouter-p1-native-registry-minimal.log`).
- Full-feature continuation/pipeline/shared/config/assembly selection
  passed **553/553**, 3,066 filtered, zero retries in 16.011 seconds
  (`/tmp/bitrouter-p1-native-registry-full.log`). Local continuation/terminal
  selection passed **102/102**, 2,151 filtered, zero retries in 1.581 seconds
  (`/tmp/bitrouter-p1-native-registry-local.log`).
- Parent review then corrected rollback reporting: an uncertain reservation or
  otherwise retained cleanup marker must return an explicit unresolved error,
  rather than `Ok(())`. The registry regression verifies that the original
  reservation survives that error and drains after confirmed closure. The
  affected minimal Registry selection passed **9/9**, 2,330 filtered, zero
  retries in 0.314 seconds (`/tmp/bitrouter-p1-native-registry-rollback.log`).
  The broader profiles above precede this final reporting-only change.
- Rust 1.97 all-feature library/test Clippy exited 0 in 1m02s with no
  Clippy-specific diagnostics (`/tmp/bitrouter-p1-native-registry-clippy.log`).
  After the rollback reporting fix, the same check exited 0 in 1m03s
  (`/tmp/bitrouter-p1-native-registry-rollback-clippy.log`), again with no
  Clippy-specific diagnostics.
  There are still 149 library and two library-test unused-code warnings from
  staged integration; this is not final strict P1 lint acceptance. No suppression
  was added. The existing dependency future-compatibility warning remains.
- Independent source review found no open findings in the bounded registry
  integration (`/tmp/bitrouter-p1-native-registry-review.json`), including the
  rollback reporting correction. The candidate manifest records source and
  evidence fingerprints (`/tmp/bitrouter-p1-native-registry-candidate.json`).
  Formatting and whitespace checks passed. Whole-generation
  resources, official process startup, supervisors, operator interfaces,
  trajectory integration and process-level acceptance remain open. No P1 gate
  is closed and no P1 PR is submitted by these internal fixtures.

## Process-owned telemetry injection (2026-10-21)

- Shared assembly now receives one process telemetry owner, including an
  explicit disabled case. Generations borrow exporter and status views without
  rebuilding local telemetry configuration, resolving account attribution or
  installing another propagator. Generation status shutdown is a no-op; only
  the process owns exporter shutdown. Assembly also rejects a namespace that
  differs from its injected continuation authority before initialization.
- The process retains both the blocking SDK shutdown handle and actual trace/
  metric processor joins. Cancelled waiters and elapsed deadlines retain that
  ownership. Forced joining aborts the processors before provider teardown and
  starts no new flush. Original provider outcomes and worker failures remain
  cached; a later call cannot turn a failed or forced result into a clean one.
  Ordinary local exporter construction and its global/environment behavior are
  preserved.
- An actual two-generation assembly fixture exposed an incomplete-owner Drop
  deadlock. Process sampling showed SDK provider Drop synchronously waiting for
  a processor receiver on the same runtime thread. The fixture process was
  terminated and the failed run remains recorded in
  `/tmp/bitrouter-p1-process-telemetry-corrected.log`. Processor futures now keep
  the exporter alive until their complete futures and receivers are destroyed;
  the runtime adapter holds a weak owner reference to avoid a cycle. A no-yield
  Drop regression verifies exporter release with metrics both enabled and
  disabled. Incomplete Drop still does not constitute a joined shutdown.
- Independent review found `PT-I02`: invalid trace queue bounds could panic
  after starting a metric reader, before the complete-exporter keepalive exists.
  Construction now validates the nonzero Tokio channel range before either
  worker starts. Metrics on/off tests cover zero, above-limit and maximum-usize
  capacities, an empty owned task set, the redacted process error, and the local
  constructor returning an error.
- Tests exercise the real two-generation assembly path, status views that cannot
  shut down shared telemetry, expired/repeated drains, forced joins, reentrant
  panic destruction and a real OTLP flush blocked by a local collector. The
  latter cancels the waiter while SDK shutdown is pending, resumes the same
  blocking task, then verifies one request and one joined processor after the
  collector releases it. A separate controlled blocking fixture checks handle
  retention and concurrent-waiter rejection.
- Final all-feature telemetry/assembly selection passed **112/112**, 3,520
  filtered, zero retries in 0.324 seconds
  (`/tmp/bitrouter-p1-process-telemetry-full-final.log`). The earlier run's sole
  failure was the collector fixture's missing `/v1/traces` endpoint path; its
  shutdown report correctly retained the resulting flush error. The full test
  image still has Darwin's compact-unwind section-size warning.
- Final gRPC-only shared telemetry selection passed **64/64**, 2,339 filtered,
  zero retries in 0.165 seconds
  (`/tmp/bitrouter-p1-process-telemetry-grpc.log`). Shared assembly without an
  OTLP feature passed **31/31**, 2,309 filtered in 0.166 seconds
  (`/tmp/bitrouter-p1-process-telemetry-no-otlp.log`). Local HTTP telemetry/
  assembly passed **84/84**, 2,225 filtered in 0.204 seconds
  (`/tmp/bitrouter-p1-process-telemetry-local.log`), both with zero retries.
- Rust 1.97 all-feature library/test Clippy exited 0 in 1m02s without
  Clippy-specific diagnostics
  (`/tmp/bitrouter-p1-process-telemetry-clippy.log`). The 163 library and one
  library-test unused-code warnings still reflect staged integration, and the
  dependency future-compatibility warning remains. This is not final strict P1
  lint acceptance; no suppression was added.
- Independent source review resolved `PT-I01` (incomplete-owner Drop) and
  `PT-I02` (partial initialization), with no remaining source finding
  (`/tmp/bitrouter-p1-process-telemetry-review.json`). The candidate manifest
  pins the source/documents and final validation logs
  (`/tmp/bitrouter-p1-process-telemetry-candidate.json`). Formatting and
  whitespace checks passed.
- These checks establish the bounded SDK processor/provider ownership contract.
  They do not join every dependency-internal HTTP/gRPC transport task or prove
  delivery of all historical telemetry. Standard SDK environment remains read
  at process exporter construction. Official startup, instrumentation setup,
  whole-generation retention, process shutdown and all P1 acceptance gates
  remain open.

## Bounded whole-generation ownership (2026-10-21)

- The manager retains complete assembled applications alongside their snapshots:
  one current, a bounded retired collection and one candidate/build/failed-cleanup
  slot. `max_retired` defaults to 4 and accepts 1–64. Publication cannot evict a
  live owner; terminal withdrawal closes all HTTP acquisition gates permanently.
- Registration allocates its producer intent before starting SQL. Creation and
  absence-close share an identity lock and a closed tombstone fences a delayed
  initializer. Failed candidates with invalid continuation keys can still close
  their own identity under deployment authority. Another live producer remains
  open. Shared assembly rejects unsupported MCP/tool/agent execution before
  starting resources; local assembly behavior remains unchanged.
- Build and drain waiters borrow retained handles. Normal retirement joins the
  build and pipeline, confirms native producer closure/reconciliation, then
  checks custody and readers. Cancelled waits retain the complete application
  and original absolute deadline. Historical failures remain visible separately
  from the proof that resources can be reclaimed.
- Independent review found `G-I01`: a single 32-entry custody chunk could miss
  reclaimable old entries behind live current requests. Reconciliation now yields
  between bounded chunks, completes a fair sweep and repeats only after progress.
  Tests cover forty busy entries before sixty-six reclaimable entries and an
  all-live sweep that returns without spinning.
- Corrected minimal shared-feature selection passed **15/15**, 2,334 filtered,
  zero retries in 0.373 seconds
  (`/tmp/bitrouter-p1-generations-corrected.log`). It also covers capacity refusal,
  complete-application lifetime, same-revision rejection, cancelled build/native
  waits, expired retries, invalid-key cleanup and failure-history preservation.
- The broader minimal shared-feature selection passed **210/210**, 2,139
  filtered, zero retries in 15.900 seconds
  (`/tmp/bitrouter-p1-generations-minimal.log`). The same selection under all
  features passed **229/229**, 3,412 filtered, zero retries in 15.960 seconds
  (`/tmp/bitrouter-p1-generations-full.log`), including real PostgreSQL and
  complete two-generation telemetry assembly. The existing Darwin compact-unwind
  linker warning remains unrelated to these ownership assertions.
- Pure local gateway continuation/assembly/configuration regressions, with no
  PostgreSQL test URL, passed **123/123**, 2,130 filtered, zero retries in 1.666
  seconds (`/tmp/bitrouter-p1-generations-local.log`).
- Independent source reviews closed `G-I01` and all three candidate construction
  findings (`CB-01`–`CB-03`). Initial Clippy found a `manual_map` simplification
  in retirement collection removal; the expression was corrected without
  changing its behavior. The final source repeated the complete all-feature
  selection: **229/229 passed**, 3,412 filtered, zero retries in 16.025 seconds
  (`/tmp/bitrouter-p1-generations-full-final.log`).
- Final Rust 1.97 all-feature library/test Clippy exited 0 in 1m03s with no
  Clippy-specific diagnostics (`/tmp/bitrouter-p1-generations-clippy.log`). The
  182 library and one library-test unused-code warnings are staged runtime
  integration gaps, not final strict lint acceptance. No suppression was added.
  Formatting and whitespace checks passed. The candidate manifest pins the
  source, documents and validation logs
  (`/tmp/bitrouter-p1-generations-candidate.json`); independent bounded reviews
  are recorded in `/tmp/bitrouter-p1-generations-review.json` and
  `/tmp/bitrouter-p1-candidate-build-map.json`.
- This stage does not implement forced process shutdown, startup/publication
  supervision or official independent-process acceptance. P1-A01–A09 remain open.

## Retained shared transport roots (2026-10-21)

- The shared transport now retains its accept handle and a separate connection
  task set. Accept cancellation or panic leaves connection ownership intact.
  Registration and force share a permanent gate, preventing insertion of a
  previously accepted socket after shutdown. Completed tasks still count toward
  the connection bound until reaped, and socket permits survive response EOF.
- Graceful waits borrow the retained handles and use one immutable absolute
  deadline. A cancelled or expired waiter does not force or discard connections.
  Force broadcasts abort to both roots before waiting, is available while a
  graceful waiter exists, and cannot be reversed into a new graceful attempt.
  The accept driver joins before the shutdown waiter reaps connections, avoiding
  competing task-set wakers. The previous relative-timeout serving function is
  now a fixture wrapper around this owner; local HTTP serving is unchanged.
- Reports preserve accept/connection cancellation and panic history separately
  from quiescence. Returned panic payloads are destroyed outside owner locks;
  an active join observer remains a barrier while those destructors execute.
  Reentrant payload fixtures check that a momentarily empty task set cannot
  report quiescence before destruction finishes.
- Tests cover real held HTTP handlers, normal completed responses, cancellation
  of both graceful and forced waits, deadline-extension rejection, closed-gate
  registration, accept cancellation, accept-reaper panic and retained siblings.
  The first compile exposed a test-helper visibility error, corrected by exposing
  the existing bounds fixture to its parent test scope. No production behavior
  was involved in that correction.
- The first transport selection passed **25/25**. After the two payload-barrier
  regressions and permanent-force guard, the minimal shared selection passed
  **34/34**, 2,325 filtered, zero retries in 0.279 seconds
  (`/tmp/bitrouter-p1-owned-transport-minimal.log`). It comprises 27 transport and
  seven generation-manager tests. Shared ingress helpers are exercised by the
  aggregate PostgreSQL test, not independently selected tests.
- Final Rust 1.97 all-feature library/test Clippy exited 0 in 1m02s, with no
  Clippy-specific diagnostics (`/tmp/bitrouter-p1-owned-transport-clippy.log`).
  The 191 library and one library-test unused-code warnings reflect staged
  integration; no suppression was added and strict P1 lint acceptance is open.
- Independent source review approved this bounded owner contract and resolved
  `OT-I01` (panic-payload destruction as an observation barrier). The narrow
  accept/gate audit also found no actionable issue. Final all-feature shared,
  continuation, pipeline-owner, assembly and configuration regressions passed
  **239/239**, 3,412 filtered, zero retries in 16.062 seconds, including the real
  PostgreSQL aggregate and its HTTP transport campaign
  (`/tmp/bitrouter-p1-owned-transport-full.log`). Formatting and whitespace
  checks passed. Files and logs are pinned in
  `/tmp/bitrouter-p1-owned-transport-candidate.json`; the bounded review is
  `/tmp/bitrouter-p1-owned-transport-review.json`.
- Process-wide force ordering, generation/native fan-out, telemetry ordering,
  startup and all P1 acceptance gates remain open. These owner fixtures are not
  independent-process shutdown acceptance or a claim of transport-internal
  dependency-task closure.

## Generation and native force integration (2026-10-21)

- Native continuation force now separates a synchronous signal from a borrowed
  join. The signal interrupts active graceful SQL waits, permanently closes the
  restart/admission gates and preserves the worker handle and durable markers.
  A fresh status reports actual joins separately from producer closure.
- The generation manager broadcasts force across current, retired and candidate
  slots. It retains failed and completed-but-unobserved builds, applies sticky
  force to newly harvested applications, and joins native/pipeline groups
  concurrently. Pipeline force requires an actually joined transport and its
  own zero-source check. It never starts forced compensation/closure SQL or
  clears unresolved custody to claim success.
- Independent source review found no production defect. Its nonblocking coverage
  observation was addressed: two PostgreSQL cases now observe the actual blocker
  PID with `pg_blocking_pids` before forcing cancellation, rather than relying on
  elapsed time. Tests also cover cancelled joins, retained source/snapshot
  barriers, late build results and failure-history preservation.
- Final minimal shared-feature regressions passed **225/225**, 2,139 filtered,
  zero retries in 15.913 seconds
  (`/tmp/bitrouter-p1-generation-force-minimal.log`). Final all-feature shared,
  continuation, pipeline-owner, assembly and configuration regressions passed
  **244/244**, 3,412 filtered, zero retries in 16.169 seconds
  (`/tmp/bitrouter-p1-generation-force-full.log`). Both explicitly include ignored
  PostgreSQL tests. The earlier lock-observed command omitted `--run-ignored all`
  and is not evidence for the database cases.
- Rust 1.97 all-feature library/test Clippy exited 0 in 1m03s with no Clippy
  diagnostics (`/tmp/bitrouter-p1-generation-force-clippy.log`). The 197 library
  and one library-test unused-code warnings still reflect runtime integration
  work; this is not final strict lint acceptance. Formatting and whitespace
  checks passed. Source/evidence identities are recorded in
  `/tmp/bitrouter-p1-generation-force-candidate.json`; the bounded independent
  review is `/tmp/bitrouter-p1-generation-force-review.json`.
- The process supervisor, official startup, workers and telemetry ordering still
  require integration. These tests do not close P1-A01–A09 or establish complete
  independent-process shutdown acceptance.

## Official foreground runtime (2026-10-21)

- `bro serve --config` now selects and retains its configuration before daemon
  discovery, home creation or CWD changes. Shared input has a strict process-only
  schema and a bounded read. Local expansion must remain local and its validated
  document is reused. The structural probe preserves local flow interpolation,
  mapping keys, anchors, aliases and merge precedence while rejecting literal
  duplicate selectors before environment lookup. The 16 MiB selection cap is an
  implementation choice recorded in the CLI and skill references.
- A retained lifecycle owns the original joined incarnation, dedicated heartbeat
  pool, publication, retirement, custody and recovery workers, all generations,
  optional HTTP transport and the process telemetry owner. Startup/listen failures
  use the same shutdown path. Local withdrawal precedes durable withdrawal; all
  waits retain one deadline, second termination requests force, and transport
  joins precede pipeline force. Reports distinguish quiescence, durable uncertainty
  and a clean exit. Windows signal handling matches the existing local host;
  its source was reviewed, but Windows execution is not claimed here.
- Independent review found and repaired stalled authority polling behind retired
  sources, stale readiness while heartbeat SQL was blocked, maintenance batches
  delaying shutdown, and local-parser compatibility/selection defects. Tests
  observe actual database lock waits, preserve unknown-ACK custody, resume
  cancelled shutdown and exercise the startup path without a transport.
- Final Rust 1.97 minimal shared regressions passed **484/484**, 1,898 filtered,
  zero retries and no leak results in 16.325 seconds
  (`/tmp/bitrouter-p1-runtime-final-minimal-v2.log`). The all-feature selection
  passed **508/508**, 3,166 filtered, zero retries and no leak results in 16.796
  seconds (`/tmp/bitrouter-p1-runtime-final-full.log`). Both explicitly include
  ignored PostgreSQL tests and cover shared runtime, continuation, pipeline
  ownership, assembly, configuration, reload and host behavior.
- Earlier fixture failures and two parallel leak results are preserved in the
  initial/corrected logs; the repaired serial run passed 11/11 and the final
  parallel matrices above are clean. A separate attempted matrix stopped after
  267 passes because its entire build directory disappeared and Nextest could
  no longer execute the binary. Its cause was not established. It is incomplete
  infrastructure evidence, not a product failure or a pass. The final runs used
  a dedicated build directory and unchanged source throughout each run.
- Final all-feature library/test Clippy exited 0 in 1m37s with no Clippy
  diagnostics (`/tmp/bitrouter-p1-runtime-final-clippy-full.log`). Its 24 unused
  library warnings identify remaining P1 integration; strict whole-product lint
  acceptance remains open. No warning suppression was introduced.
- Local `--no-default-features` regressions passed **372/372**, 1,889 filtered,
  zero retries and no leak results in 2.141 seconds
  (`/tmp/bitrouter-p1-runtime-final-local.log`). The same profile's library/test
  Clippy passed with `-D warnings` in 45.73 seconds
  (`/tmp/bitrouter-p1-runtime-final-clippy-local.log`). Formatting and whitespace
  checks passed. The final source/log manifest is
  `/tmp/bitrouter-p1-runtime-candidate.json`; bounded independent reviews are
  `/tmp/bitrouter-p1-runtime-review.json` and
  `/tmp/bitrouter-p1-startup-publication-review.json`.
- These are bounded runtime tests, not independent gateway-process acceptance.
  Operator bootstrap/API, durable metering/trajectory projection and guard
  catch-up, retained-key correlation, outbox delivery, complete readiness details,
  local-launcher guards, official shared artifacts and P1-A01–A09 remain open.

## Official process startup and local launcher boundary (2026-10-21)

- Local `start`/`restart` now reject shared mode before daemon discovery,
  replacement, home/runtime-file creation or child spawning. The common
  `daemon::start_and_wait` entry applies the same guard for automatic launchers.
  The CLI and shipped skill document foreground service ownership.
- Three opt-in PostgreSQL fixtures run the actual Cargo-built `bro`, with
  separately retained child processes, explicit configuration/key inputs and
  isolated working/home paths. They verify two independently joined replicas,
  unauthenticated inference rejection, clean idle SIGTERM exits with the other
  replica still ready, listener-bind failure, unpublished-namespace rejection,
  and rejected `start`/`restart`/managed-child paths. Durable owner rows and
  filesystem contents are checked. Cleanup signals only children created by the
  fixture and waits for every retained child before closing its database pool.
- The candidate executable was built using Rust 1.97 with
  `--no-default-features --features multi-instance`; its build-info profile is
  still `custom`, so this does not close the official distribution-profile task.
  Artifact metadata is in `/tmp/bitrouter-p1-official-process-artifact.json`.
  `/tmp/bitrouter-p1-official-process-initial.log` records **71/71** selected
  library/binary tests passing, 2,373 filtered, zero retries and no leak results
  in 1.990 seconds. This includes all three real process fixtures and local
  host/daemon/restart regression cases.
- The unshared profile's same local selection passed **65/65**, 2,255 filtered,
  zero retries and no leak results in 1.142 seconds
  (`/tmp/bitrouter-p1-launcher-local-tests.log`). Library/binary/test Clippy
  passed with `-D warnings` locally in 27.62 seconds and with all features in
  1m04s; the latter retains 24 staged unused-code warnings and no Clippy
  diagnostics. Formatting and whitespace checks passed. The source, five logs,
  artifact metadata and executable digest are pinned in
  `/tmp/bitrouter-p1-official-process-candidate.json`. Independent reviews are
  `/tmp/bitrouter-p1-official-process-review.json` and
  `/tmp/bitrouter-p1-local-daemon-guard-review.json`.
- These fixtures use internal schema/authority setup. They do not establish
  bootstrap through stock operator commands, authenticated accepted-work drain,
  crash recovery or any complete P1 gate. All P1-A01–A09 remain pending.

## Atomic operator commands and receipts (2026-10-21)

- Added pure `bitrouter-ai::gateway::operator` inputs, grants, metadata and
  receipts. Secret-bearing write diagnostics are redacted; stored credentials,
  deployment keys and virtual-key verifiers have no read/receipt projection.
  Protocol interpretation and transaction execution remain in the application.
- An application-owned operator facade implements all seven command variants:
  tenant/resource policy, global principal administration, credential import,
  configuration publication, correlation rotation and Unknown resolution. It
  does not join a serving incarnation. Existing engine entry points delegate to
  the same transaction helpers. Publication validates authenticated credential
  contents under its exclusive head fence; metadata alone cannot authorize it.
- Each mutation and its actor-bound receipt commit together. A private keyed
  fingerprint binds the complete input without retaining a plaintext secret
  digest. Identical replay returns the original receipt after a current grant
  check; conflicting input fails. Missing receipts remain unresolved. Policy CAS
  preserves occupancy, credential CAS preserves resource identity, and config
  and key generations advance independently. Global principal access has a
  separate explicit capability and locks the existing database-wide key row.
- A two-pool PostgreSQL campaign exercises all commands, concurrent replay/CAS,
  original receipt recovery, grant revocation, actor/namespace isolation,
  invalid-input rejection before blocked SQL, transaction visibility/rollback,
  occupancy preservation and secret-free receipt rows. Corrupt ciphertext,
  authenticated invalid credential encoding and started refresh all reject
  publication without a new head, configuration row or successful receipt.
  Unknown resolution preserves its atomic permit release/projection/outbox work.
- Frozen-source validation used Rust 1.97 and the dedicated runtime target.
  The all-feature shared selection passed **69/69**, 3,610 filtered, in 14.061s
  (`/tmp/bitrouter-p1-operator-final-full-tests.log`); the minimal shared profile
  passed **67/67**, 2,320 filtered, in 14.112s
  (`/tmp/bitrouter-p1-operator-final-minimal-tests.log`). Both include ignored
  PostgreSQL cases, with zero retries and no leak results. Actual process tests
  were excluded because the previously built executable predates this stage.
- AI contract tests passed **47/47**; doctest execution found zero tests
  (`/tmp/bitrouter-p1-operator-final-contract-tests.log`). Standalone all-feature,
  all-target AI Clippy passed with `-D warnings` in 6.50s. All-feature application
  library/test Clippy exited 0 in 1m08s with no Clippy diagnostics; 61 staged
  unused-code warnings remain while operator and other P1 integration is open.
  No suppression was added. The full test binary emitted the macOS large unwind
  table linker warning; logs also retain the existing proc-macro-error2 future
  incompatibility warning. Neither is a performance result. Formatting and
  whitespace checks passed.
- The source and five final logs are pinned in
  `/tmp/bitrouter-p1-operator-candidate.json`. Bounded reviews are recorded in
  `/tmp/bitrouter-p1-operator-foundation-review.md` and
  `/tmp/bitrouter-p1-operator-backend-review.md`. The two source findings were
  repaired: pre-SQL identity validation and full credential validation during
  publication. Receipt recovery after a discarded successful reply is synthetic
  evidence, not an injected PostgreSQL lost-COMMIT-ACK test. Cancellation coverage
  here is before domain writes; later-write cancellation remains a process
  campaign obligation, distinct from the explicit transaction rollback test.
- This stage exposes no operator HTTP/CLI/SDK entry point and does not establish
  stock bootstrap, transport authentication, whole-product strict lint, durable
  projection delivery or independent-process acceptance. All P1-A01–A09 remain
  pending; P0 is unchanged.

## Stock atomic bootstrap (2026-10-21)

- `bro shared init` now uses explicit process/seed inputs and caller identities,
  validates before database work, and seeds a namespace in one transaction after
  serialized schema installation. The initial publication, keys, global
  principal inserts and all receipts commit or roll back together. Identical
  replay returns the original receipt without resetting later administration.
  Local configuration is rejected before local environment expansion; named
  contexts and unavailable features reject before input or logging setup.
- Independent source reviews closed BCLI-I01 (local configuration could disclose
  expanded secrets), BCLI-I02 (context validation after global tracing setup),
  and BPF-I01 (a test URL `dbname` query could override its owned database).
  The new fixture strips those overrides and checks `current_database()` before
  any schema work. Only a UUID database created by the fixture is dropped.
- Rebuilt stock executable: Rust 1.97.0, no default features plus
  `multi-instance`, dev/test debug info disabled, incremental disabled, two build
  jobs. SHA-256 `f0e4b17c3302a0d1d5ef9f38ff555329d3d2f51318d8525c1c8785a5a7033f7a`.
  The binary includes the final production source; subsequent changes were test
  isolation checks and documentation. Its build log is
  `/tmp/bitrouter-p1-bootstrap-final-binary-build.log` (34.75s).
- Frozen-source checks in the dedicated target directory:
  `/tmp/bitrouter-p1-bootstrap-final-minimal-tests.log`: **90/90**, 2,365 outside
  selection, 14.221s; `final-full-tests.log` with the same filename prefix:
  **92/92**, 3,665 outside selection, 15.982s. Both include four actual stock
  process cases and have no retry or LEAK results. The new case bootstraps a
  genuinely empty database, starts two initializers, checks equal replay
  receipts/no serving owner, joins two independent replicas and checks idle
  graceful exits/no local daemon state. It is not deterministic lock-contention
  proof or authorized inference/accepted-work drain evidence. A post-run catalog
  check found no remaining fixture-created database.
- Local `--no-default-features` regression selection: **11/11**, 2,312 outside
  selection, 0.048s (`final-local-tests.log`). The first local command mistakenly
  named nonexistent `db-sqlite` and stopped before compilation; its separate log
  is retained. SQLite is a base dependency, not a feature flag.
- Public contracts: **47/47**, zero doctests (`final-contract-tests.log`). Strict
  contract Clippy and strict local application lib/bin Clippy passed
  (`final-contract-clippy.log`, `final-local-clippy.log`). All-feature application
  lib/bin/tests Clippy passed with no Clippy diagnostics and 25 staged unused
  library warnings (`final-clippy.log`); this is not whole-product warning-free
  acceptance. Formatter/diff checks passed. The existing proc-macro-error2
  future-incompatibility notice and macOS large-unwind-table linker warning are
  retained in build logs.
- Earlier intermediate backend testing reported one LEAK in the existing
  operator test. Two immediate unchanged-source reruns and the final minimal/full
  selections were clean; the first cause remains unestablished. The first CLI
  boundary run failed on the global-subscriber issue above; its production
  preflight fix is covered by both final profiles. These initial failures are
  retained as history, not substituted for final evidence.
- Remote operator API/authentication/SDK/CLI, durable projection integration,
  official shared distribution, aggregate checks/performance and all P1-A01–A09
  gates remain open. The empty seed process fixture does not establish principal
  or credential seeding via CLI, crash recovery, lost database COMMIT ACKs or
  complete process teardown under arbitrary harness termination. P0 is unchanged.
- Final independent bootstrap backend/process and CLI reviews approved the frozen
  candidate with zero open blocking findings. The documentation precision note
  was applied: accepted input is 2 MiB, with one extra byte read for overflow
  detection. Candidate manifest: `/tmp/bitrouter-p1-bootstrap-candidate.json`.

## Authenticated operator transport and SDK (2026-10-21)

- Shared processes can explicitly enable a separate loopback operator listener.
  Literal actor/grant/environment-reference validation precedes environment
  lookup; token snapshots precede database pools. Authentication precedes body
  reads. Requests use bounded JSON and a whole-handler deadline; writes that time
  out remain Unknown. The listener is independent of inference readiness during
  operation and participates in the retained process shutdown alongside inference.
- `OperatorClient` supplies explicit mutation, actor-scoped receipt, revision and
  secret-free metadata calls. It performs no discovery, automatic replay or
  redirects. Response identities/media/size are checked, and automatic decoding
  is disabled even under dependency feature unification. SDK errors distinguish
  definite rejections from unknown write outcomes and retain only the original ID.
- Source review repairs covered whole-request deadlines, complete media parsing,
  Bearer grammar, SDK fixture task ownership and compressed-response validation.
  A streamed-body size test was strengthened to use a valid oversized receipt
  and require the specific Limit error, removing its invalid-JSON false positive.
- The first process campaign passed 32/33: its readiness-fault setup was rejected
  by configuration validation before producing a fault. The repaired fixture
  atomically modifies an immutable publication together with its new head; it is
  explicit storage corruption, not an input accepted by the public API. The
  timeout test now observes the POST's actual PostgreSQL blocker and asserts a
  600 ms watchdog around its 200 ms handler budget. GET and pending-body timeout
  branches have the same watchdog; only POST has the observed SQL-wait barrier.
- Minimal shared integration recheck: **33/33**, 2,370 outside selection, 1.299s
  (`/tmp/bitrouter-p1-operator-service-integration-recheck.log`). Final full-feature
  lib/bin selection: **35/35**, 3,731 outside selection, 2.376s
  (`/tmp/bitrouter-p1-operator-transport-final-full-tests.log`). Both are clean
  first-attempt runs without LEAK results. Actual stock processes exercise
  cross-replica receipt replay/query, Unicode query identities, separate inference
  routes, readiness loss/recovery, invalid-token startup and both bind failures.
  The lifecycle case cancels/resumes the original deadline and joins two held
  connections. Exact operator-join-before-pipeline-force ordering is established
  by the reviewed call sequence, not an independent test ordering sentinel.
- Final SDK all-feature tests with `reqwest/gzip` and the positive decoder control:
  **93 tests + 6 doctests passed**; strict all-target SDK Clippy passed. Logs share
  `/tmp/bitrouter-p1-operator-transport-final-` with suffixes `sdk-tests.log` and
  `sdk-clippy.log`. Local no-default lib/bin selection: **10/10**, 2,313 outside
  selection, 0.040s (`local-tests.log`); strict local lib/bin Clippy passed
  (`local-clippy.log`). All-feature app lib/bin/tests Clippy passed with no Clippy
  diagnostics and 18 staged unused library warnings (`app-clippy.log`). These
  warnings still prevent whole-product warning-free acceptance. Existing
  proc-macro-error2 and macOS unwind-table notices remain recorded.
- The final stock executable uses Rust 1.97.0, no default features plus
  `multi-instance`, disabled dev/test debug info and incremental compilation,
  and two build jobs. SHA-256:
  `352cbd9b4a30599545d0b357c165a5b50aad5f65b935c21fe6b903d309adc0b4`.
  Its build completed in 35.03s (`final-binary-build.log` with the same prefix).
  Source, binary, commands and log hashes are recorded in
  `/tmp/bitrouter-p1-operator-transport-candidate.json`.
- Independent SDK and service reviews approved the final bounded candidate with
  zero open findings. Reports are `/tmp/bitrouter-p1-operator-sdk-review.md` and
  `/tmp/bitrouter-p1-operator-service-review.md`. Remote operator CLI, durable projection/outbox integration, shared
  distribution, full aggregate checks/performance and P1-A01–A09 remain open.
  This stage does not prove a committed mutation followed by lost HTTP/SQL ACK,
  exact operator-driver failure injection, arbitrary process-crash cleanup or
  accepted inference draining. P0 is unchanged.

## Remote operator CLI (2026-10-21)

- Stock `bro shared` commands now apply literal JSON mutations and query operation receipts, revisions and secret-free metadata through the HTTP SDK. Every command requires an explicit endpoint and token environment reference; it performs no discovery, local state initialization, automatic replay or fallback query. Mutation input is bounded to 2 MiB, with one extra byte read for overflow detection.
- Unknown write outcomes retain the original operation ID and a recovery hint. Unresolved lookup is a successful observation, not rollback evidence. Typed CLI error adaptation suppresses transport sources and private outer context. CLI reference, specification and shipped skill were updated together.
- Independent source review found and resolved two boundary defects: Clap printed invalid token-reference arguments before redaction, and space-separated recovery arguments misparsed valid leading-hyphen operation IDs. Runtime validation now precedes lookup, and hints use `--operation-id=ID`. Tests also require exact preflight errors, otherwise-valid duplicate/unknown-field inputs, HTTP methods and unchanged request counts for invalid IDs or missing files.
- Repaired minimal shared selection passed **17/17**, 2,458 outside selection, 2.146s. The final full-feature CLI/skill-contract selection passed **18/18**, 3,760 outside selection, 3.459s; this includes the two later zero-request cases. Actual stock CLI processes submit to replica A, query the exact receipt and Unicode resource through B, verify unresolved JSON/human exit 0, and check both output streams for malformed-input/token-reference/non-Unicode-token disclosure. No local home state is created.
- Final no-default lib/bin selection passed **10/10**, 2,317 outside selection, 0.067s. Strict local lib/bin Clippy passed in 17.56s. All-feature app lib/bin/tests Clippy passed in 1m04s with no Clippy diagnostics and 18 staged unused-code warnings. These remaining integration warnings are not final warning-free acceptance. Formatter/diff checks passed. The existing proc-macro-error2 future-incompatibility and macOS unwind-table notices remain recorded.
- Evidence logs use `/tmp/bitrouter-p1-operator-cli-`: `repaired-tests.log`, `repaired-binary-build.log`, `final-full-tests.log`, `final-local-tests.log`, `final-clippy.log` and `final-local-clippy.log`. The Rust 1.97.0 minimal shared executable build completed in 34.93s; its SHA-256 is `e94428d662e256baf61877a691a8c4b5d2cb4306db137d4d4d2940854fa61315`. This is the historical minimal-build fingerprint in `operator-cli-source.json`; the later full-feature integration build replaced the common target path. The current full-feature binary is retained at `/tmp/bitrouter-p1-operator-cli-full-bro`, SHA-256 `a2c44bdcedea6d19b44cf3e390a7a4e515d55c59f88b19c79f01139352edbcc0`. It matches Cargo's `bro-43e68cce2609d5ba` executable and all-feature fingerprint; the final candidate distinguishes both artifacts.
- Independent review approved the final bounded candidate with zero open source/evidence findings (`/tmp/bitrouter-p1-operator-cli-review.md`). The artifact-provenance correction preserves the historical minimal hash and pins the subsequent full-feature executable separately. This bounded stage does not test a lost database COMMIT acknowledgement, CLI disconnect/redirect injection or post-apply stdout failure. Durable projection/outbox integration, official shared distribution, aggregate checks/performance and all P1-A01–A09 gates remain open. P0 is unchanged.

## Durable attempt accounting evidence (2026-10-21)

- Attempt preparation now retains the original provider/model/protocol and frozen tariff in the same transaction as reservation. Dispatch rejects missing attribution. The first terminal observation retains normalized billing counters, charge evidence and optional observed attempt lifetime; it excludes provider raw usage, bodies and diagnostics. Unknown usage and missing prices remain explicit unknowns. Lifetime includes local preparation/persistence and is not upstream latency.
- The first terminal snapshot is cached before persistence and shared with custody recovery. Cancellation and Drop reuse those exact facts, including lifetime. Terminal facts, public usage, capacity release, execution mirror and the pending episode fence commit together. Exact replay compares values; changed pricing/lifetime is rejected. Historical missing evidence is not silently repaired.
- Independent source review closed AE-I01: an admitted near-64-KiB tariff could expand beyond the former equal terminal limit. Terminal facts now have a derived 135,168-byte bound; both unit and real PostgreSQL cases persist near-boundary attribution. AE-T01 strengthened cancellation verification to compare the actual cached first observation after an observed SQL blocker, cancellation and delayed Drop.
- Large evidence payloads are boxed, and the runtime snapshot is shared through Arc. The PostgreSQL foundation's executor campaign runs in an owned, joined task to avoid excessive nested async poll stack. No thread-stack increase or lint suppression was used. Intermediate logs retain three stack-abort runs, one custody-cleanup assertion failure and transient LEAK results; their presence is not final passing evidence.
- Final minimal shared selection passed **5/5**, 2,404 outside selection, 14.289s (`/tmp/bitrouter-p1-attempt-evidence-final-recheck-tests.log`). Final all-feature shared selection passed **82/82**, 3,619 outside selection, 15.196s (`final-full-tests.log` with the same prefix). Both completed cleanly without retries or LEAK results. They cover real PostgreSQL replay/conflicts, missing-attribution rejection, capacity accounting, cancellation recovery, mixed existing shared-runtime regressions and immutable original tariff evidence.
- All-feature app library/test Clippy passed in 1m04s with no Clippy diagnostics and 18 staged unused-code warnings (`final-clippy.log`). Strict no-default local library/binary Clippy passed in 17.59s (`final-local-clippy.log`). These staged integration warnings still preclude final warning-free acceptance. Existing dependency future-incompatibility and linker notices remain in logs. Formatting and whitespace checks passed.
- Source and final logs are pinned in `/tmp/bitrouter-p1-attempt-evidence-candidate.json`. Independent final review approved the bounded implementation, documentation and candidate evidence with zero open findings (`/tmp/bitrouter-p1-attempt-evidence-review.md`). This stage builds no new stock executable and excludes independent process tests. Namespace-scoped metering/trajectory/evaluation, projection receipts, atomic guard catch-up, retained outbox delivery, aggregate checks/performance and all P1-A01–A09 gates remain open. P0 is unchanged.

## Namespace-scoped metering foundation (2026-10-21)

- Added a separate shared metering table keyed by namespace/execution, with namespace-scoped event and canonical-request uniqueness and tenant/principal admission-time indexes. The ordinary local table and migration ledger are unchanged. A borrowed transaction inserts or verifies an immutable execution record; no method commits, retries a partial operation or starts a background task.
- Each typed record retains up to 32 original attempts, with exact source event/revision, principal, optional canonical request binding and original database times. The row and its known/unknown accounting columns derive from one checked fold. It preserves fallback attribution, mixed usage provenance, missing prices, real no-dispatch identities and operator assertions without inventing a zero charge. Its explicit maximum serialized size is 6,701,056 bytes. Namespace and owner/window filters precede spend aggregation; the result covers projected rows, not pending authority.
- The existing attempt evidence implementation and usage/outcome reducers moved beside metering. Gateway finalization and projection use those same reducers. A private engine loader reads immutable terminal/event/attempt evidence in the caller's transaction without reversing execution/episode lock order; an actual finalized execution exercises that loader and the new adapter. This stage does not activate the receiver or replace serving recorders.
- Initial minimal shared tests passed **11/11**, 2,404 outside selection, 14.273s. Self-review added exact terminal-disposition checks, a regression for conflicting operator assertions, and real database coverage for both resolution outcomes plus a no-attempt row. The revised minimal selection passed **12/12**, 2,404 outside selection, 14.236s. Both runs were clean without retries or LEAK results. Logs are `/tmp/bitrouter-p1-shared-metering-initial-tests.log` and `recheck-tests.log` with the same prefix.
- PostgreSQL fixtures verify borrowed-transaction rollback, two-connection duplicate convergence, exact replay/conflicts, owner filtering, half-open admission windows, matching identities in two namespaces and the unchanged local table, known subtotals with unknown costs, explicit no-dispatch presence and rejection of inconsistent stored summary columns on exact read. Concurrent duplicate convergence does not itself establish a deterministic SQL-lock barrier or lost-COMMIT-ACK injection.
- Before the final review repair, the all-feature shared/metering selection passed **174/174**, 3,534 outside selection, 14.935s (`/tmp/bitrouter-p1-shared-metering-full-tests.log`). No-default local metering passed **85/85**, 2,178 outside selection, 0.360s (`local-tests.log` with the same prefix). Independent review found SM-I01: floating-point equality could equate differently serialized frozen zero rates. Replay now additionally compares the exact canonical payload. A real PostgreSQL regression distinguishes valid `0.0` and `-0.0` evidence, retains the original row and rejects the changed input. Added coverage also persists/replays 32 boundary-sized attempts and rejects an oversized whole record without insertion. Repaired minimal tests passed **12/12** in 13.960s and full-feature tests **174/174** in 15.000s (`repaired-tests.log`, `final-full-tests.log`). Both were clean. Clippy then rejected helper placement after the moved test module; only declaration order was repaired. The failed lint log is retained as `final-clippy.log`. Source fingerprints before that order-only repair are `/tmp/bitrouter-p1-shared-metering-final-source.json`; no stock executable or performance result is asserted. Native-cost queries, trajectory/eval namespaces, projection receipts/guard catch-up, receiver/outbox assembly and all P1-A01–A09 remain pending. P0 is unchanged.
- After the declaration-order repair, full-feature assertions passed twice (**174/174**, 14.815s and 15.985s), but nextest marked two bootstrap cases in the first run and a different authentication case in the second as LEAK. Logs are `verified-full-tests.log` and `final-clean-tests.log` (the latter filename is not a clean-result claim). Three unchanged serial bootstrap reruns were clean **2/2** each. The same full selection at `--test-threads 2` passed **174/174** cleanly in 17.948s (`bounded-concurrency-tests.log`). Leak detection was not disabled or relaxed; the default-concurrency cause remains unestablished. These flagged tests launch no OS subprocess in the inspected paths; the authentication case awaits its spawned Tokio task. This observation does not prove the environment caused the flags or close aggregate teardown acceptance. See [nextest's output-handle detection contract](https://www.nexte.st/docs/features/leaky-tests/).
- Final all-feature library/test Clippy passed in 1m04s with no Clippy diagnostics and **31 staged unused-code warnings** (`verified-clippy.log`); strict local library/binary Clippy passed in 17.55s (`verified-local-clippy.log`). The increased staged count includes the not-yet-injected metering adapter/loader; no warning suppression was added. Full-product warning-free acceptance remains open. Final no-default local metering passed **85/85** in 0.358s (`verified-local-tests.log`). Formatting and whitespace checks passed. Independent bounded implementation, documentation and candidate-evidence review approved the stage with zero open source findings (`/tmp/bitrouter-p1-shared-metering-review.md`). The default-concurrency LEAK observations remain unresolved validation limitations. `/tmp/bitrouter-p1-shared-metering-candidate.json` pins current files, commands and logs.

## Shared trajectory persistence (2026-10-21)

- Added separate namespace-keyed trajectory episode/event/request/prefix/outbox tables and borrowed-transaction storage. Existing local tables and pure reducer behavior remain unchanged. Start, complete route/guard batch, settlement and exact outbox reads are bounded and owner scoped; mutations require PostgreSQL Serializable isolation and caller-held episode serialization.
- Initial PostgreSQL regression failed on a duplicate event because its concurrent raw-start callers omitted the documented outer episode fence. The repaired fixture locks and advances a versioned authority row and retries the whole transaction; this is fixture-level serialization, not evidence that the unfinished serving coordinator already holds its real fence. The failed log remains `/tmp/bitrouter-p1-shared-trajectory-initial-tests.log`.
- Independent source review found ST-I01: a committed route batch could not be replayed. Exact replay now compares the complete persisted route/guard footprint before mutable phase gates and never reapplies a hold. Missing guards and changed content fail. Shared history additionally validates the semantic reducer and head endpoint timestamps, and terminal replay checks the pointed-to request identity.
- Repaired minimal shared plus existing trajectory-store tests passed **48/48**, 2,369 outside selection, 1.294s (`/tmp/bitrouter-p1-shared-trajectory-repaired-tests.log`). This includes actual PostgreSQL namespace/local isolation, concurrent raw starts and settlements, outer rollback, original-prefix settlement replay after later history, typed route/guard batches, exact hold preservation and oversized stored history/outbox rejection. Further final-candidate tests and review are still pending; this is not P1 acceptance.
- The first all-feature selection used a substring match for `trajectory::`, unintentionally including ignored `acp_trajectory` MySQL cases. It stopped after 10 passing tests when a MySQL fixture correctly rejected the PostgreSQL test URL; 253 selected tests did not run. The log remains `/tmp/bitrouter-p1-shared-trajectory-full-tests.log`. The corrected selection anchors `^trajectory::`; no test behavior or database requirement was weakened.
- Parent self-review tightened the shared JSON contract: byte preflight measures stored bytes, whereas candidate validation serializes typed events. Requiring canonical stored event JSON prevents padded representations from making an accepted append unreadable on its next bounded read. Added a padded-JSON regression, a valid 4,096-event read/exact replay with rejected 4,097th append, namespace-colliding outbox contents, delivered-outbox replay preservation, and rollback after a late outbox uniqueness failure. These refinements await the final run below.
- The corrected final all-feature shared/trajectory selection passed **229/229**, 3,480 outside selection, 17.789s at `--test-threads 2`; PostgreSQL trajectory completed in 1.508s and foundation in 13.523s. The run was clean with no retries or LEAK results (`/tmp/bitrouter-p1-shared-trajectory-final-full-tests.log`). Outer test concurrency remains bounded as documented in the preceding stage; this does not establish the earlier default-concurrency LEAK cause. Source did not change during compilation or execution.
- All-feature library/test Clippy passed in 1m04s with no Clippy diagnostics and **40 staged unused-code warnings** (`/tmp/bitrouter-p1-shared-trajectory-final-clippy.log`). The added storage helpers are not yet injected into serving, so this is not whole-product warning-free acceptance. Existing dependency future-incompatibility and macOS unwind-table notices remain recorded. No new stock executable or performance result is asserted for this stage.
- Final minimal shared selection passed **48/48**, 2,369 outside selection, 2.775s; the PostgreSQL case completed in 1.545s (`/tmp/bitrouter-p1-shared-trajectory-final-minimal-tests.log`). No-default local trajectory tests passed **139/139**, 2,124 outside selection, 2.767s (`final-local-tests.log` with the same prefix). Both runs were clean. The shared-only module/DDL remain feature gated, while the local helper changes only widen sibling visibility to reuse existing logic.
- Strict no-default local library/binary Clippy and final formatting/whitespace checks passed (`/tmp/bitrouter-p1-shared-trajectory-final-local-clippy.log`, `final-format.log`). The frozen candidate and evidence are collected in `/tmp/bitrouter-p1-shared-trajectory-candidate.json`; independent final review approved the bounded implementation, documentation and evidence with zero open source findings (`/tmp/bitrouter-p1-shared-trajectory-review.md`). No wire-level lost-COMMIT-ACK, precise contention barrier, cancellation at every SQL boundary, exact 16 MiB valid-history boundary or terminal-headroom dispatch guarantee is claimed. Evaluation storage, retained-epoch correlation/binding, projection receipts, guard catch-up, receiver/worker assembly, aggregate checks/performance and P1-A01–A09 remain open. P0 is unchanged.

## Shared evaluation persistence (2026-10-21)

- Added four independent namespace-keyed evaluation tables and a transaction-only adapter outside Core. The narrow trusted trajectory method reuses the existing envelope and built-in metric validators; the latter moved unchanged from Core admission to trajectory evaluation with explicit Core/multi-instance gating. Subject/result/actual Admitted records share the caller's Serializable transaction and subject lock. Existing local tables, stores and publisher behavior remain unchanged.
- Exact result replay validates content/indexes and returns the latest admission without appending or re-admitting. Missing admission beside an existing result fails. Namespace/owner predicates precede the latest-admission fold. Input/payload JSON is bounded to 1 MiB. The adopted schema reserves a snapshot table; production snapshot helpers are deferred as noted below.
- The initial minimal shared real-PostgreSQL fixture passed **1/1**, 2,417 outside selection, 0.320s (`/tmp/bitrouter-p1-shared-evaluation-initial-tests.log`). It covers outer rollback, two-connection duplicate application, same identities in two namespaces, foreign-owner rejection, exact admission retention, dispute replay without re-admission, historical snapshots, missing/conflicting selections, immutable field corruption, and missing-admission rejection. Subsequent additions exercise a valid exactly-1-MiB envelope and one-byte overflow; full-feature local-table parity and final checks remain pending.
- Full-feature shared/evaluation regression passed **128/128**, 3,582 outside selection, 19.131s at `--test-threads 2` (`/tmp/bitrouter-p1-shared-evaluation-full-tests.log`). The PostgreSQL evaluation fixture passed in 0.916s, including local-table parity and the valid 1-MiB/one-byte-over boundary; shared foundation passed in 13.777s. No retries or LEAK results occurred. The earlier default-concurrency LEAK cause remains unresolved; this bounded run does not close aggregate teardown acceptance.

- Reachability review found that snapshot freezing/lookup are consumed by the local Core evaluation API/compiler, while the shared gateway returns to its separate runtime before those routes are mounted. Publication itself needs subject/result/admission only. The preliminary snapshot helpers and their tests were therefore removed instead of adding unused production APIs or expanding the HTTP scope; their draft remains `/tmp/bitrouter-p1-shared-evaluation-deferred-snapshot.rs`. Earlier logs above test the broader preliminary candidate and are retained as historical evidence. The final candidate must be rechecked after this scope correction; the reserved schema uses the adopted nonnegative count constraint.
- A fresh-database run of the narrowed candidate exposed PostgreSQL SQLSTATE 40001 in the existing `unobserved_close_result_and_post_lock_evidence_are_recoverable` continuation fixture: 37 passed, one failed and 90 selected tests did not run (`/tmp/bitrouter-p1-shared-evaluation-final-full-tests.log`). The raw borrowed writer transaction lacked whole-transaction retry. The fixture now uses the existing proven-abort retry policy for that writer only; it returns the pending inspection and a fresh per-attempt evidence key after commit, then awaits/reads/commits inspection outside the retry boundary. Confirmed writes cannot be replayed because of a later inspection failure, and all lock/evidence assertions remain. The original failure does not identify its exact SQL statement; added phase context improves subsequent diagnostics. No production retry or isolation setting changed.
- The provisional wider fixture wrapper passed **128/128**, 3,582 outside selection, 19.088s on another fresh dedicated database (`/tmp/bitrouter-p1-shared-evaluation-rechecked-full-tests.log`). That run validates fresh schema application but precedes the final writer-only retry refinement, so it is not the final candidate result. Final source and checks remain pending below. Both dedicated databases are retained as test evidence; no existing database was reset.
- The refined final source passed the all-feature shared/evaluation selection **128/128**, 3,582 outside selection, 19.090s (`/tmp/bitrouter-p1-shared-evaluation-verified-full-tests.log`). Minimal shared evaluation/continuation/local-domain selection passed **25/25**, 2,393 outside selection, 0.963s (`verified-minimal-tests.log` with the same prefix). Both used two test threads and completed without retries or LEAK results. The evaluation PostgreSQL case completed in 0.952s in the latter run.
- Final all-feature library/test Clippy passed in 1m05s with no Clippy diagnostics and **50 staged unused-code warnings** (`/tmp/bitrouter-p1-shared-evaluation-verified-clippy.log`). Minimal shared production library/binary checking passed in 18.07s with **52 staged unused-code warnings** (`verified-minimal-production-check.log` with the same prefix), confirming the adapter/validator compile without Core. Integration and whole-product warning-free acceptance remain open. No new stock executable or performance result is claimed.
- Final no-default local evaluation tests passed **23/23**, 2,240 outside selection, 0.116s; strict local library/binary Clippy passed in 17.01s. Formatting and whitespace checks passed (`/tmp/bitrouter-p1-shared-evaluation-verified-local-tests.log`, `verified-local-clippy.log` and `verified-format.log`). Dependency future-incompatibility and Darwin unwind notices remain recorded. The frozen candidate pins source, commands and evidence in `/tmp/bitrouter-p1-shared-evaluation-candidate.json`; independent final review approved the bounded source, documentation and evidence with zero open findings (`/tmp/bitrouter-p1-shared-evaluation-review.md`). The scope does not include shared snapshots, receiver/guard integration, process acceptance, performance, or any completed P1-A01–A09 gate.

## Original projection input capture (2026-10-21)

- Admission now stores a private versioned projection contract derived from the original published config and embedded policy lock. It preserves mandatory metering, conditional guarded trajectory/evaluation, and selected untracked request-subject obligations independently of Core. Idempotent duplicates retain their original contract; legacy absent inputs remain absent.
- Policy recording returns its actual typed decision directly while preserving the local pending cache. Shared custody retains that handoff and bounded routing receipts synchronously at their production points. Explicit unbound/Decisions/fixed-native bypass, incomplete selection, and selected treatment stay distinct. Ready input is immutable across attempts; prepare merges it in the existing capacity transaction. Finalization seals partial or ready evidence with terminal accounting/outbox publication. No extra registry, background writer or provider-I/O transaction is introduced.
- The 64-KiB escaped-JSON budget reserves 128 bytes before terminal sealing. Before provider occupancy, captured identity, route receipts, effective effort and plan model are checked. Persisted dispatch revalidates original identity/readiness. This slice does not retain optional terminal response classifications, establish rotation-safe guard/start atomicity, switch settlement recorders or activate projection delivery.
- Initial production minimal shared library checking passed in 16.52s (`/tmp/bitrouter-p1-capture-initial-check.log`), with 53 staged unused-code warnings. The new test-only local recorder wrapper was subsequently gated with `cfg(test)`. The first test compilation exposed two fixture ownership errors after Admission gained an owned contract; those fixtures now explicitly clone the contract in struct-update expressions (`initial-tests.log` with the same prefix). No production lifetime workaround was added.
- Repaired minimal shared/policy selection passed **132/132**, 2,291 outside selection, 14.344s (`/tmp/bitrouter-p1-capture-recheck-tests.log`). Expanded capture plus real PostgreSQL foundation passed **6/6**, 2,417 outside selection, 14.949s (`custody-tests.log`). The latter verifies actual policy selection, cache removal, a duplicate carrying different requirements, an observed PostgreSQL blocker, cancelled finish and exact retained partial evidence with zero attempts. It does not publish a replacement serving generation or inject a wire-level lost COMMIT acknowledgement.
- Six additional pure policy tests cover frozen name/digest/guard checks, normalized private-content exclusion, no-tier failure, cache-independent handoff and cold-adoption rejection. Full-feature shared/policy/pipeline regression passed **603/603**, 3,118 outside selection, 21.162s (`/tmp/bitrouter-p1-capture-full-tests.log`). An unchanged-source rerun also passed **603/603**, 20.950s (`final-full-tests.log`); its filename does not signify final validation. Both were clean at two test threads. Initial all-feature library/test Clippy passed in 1m09s with no Clippy diagnostics and 50 staged unused-code warnings (`initial-clippy.log`).
- Final coverage adds dispatch receipt/effort/model mismatch and duplicate-kind rejection, actual variant selection/base fallback, and exact original-input comparison after multiple attempts. The first 604-test selection failed in the new validator fixture because its constructor was given an execution ID as the model; the fixture now supplies the prompt model and assigns request ID separately. The failed run remains in `/tmp/bitrouter-p1-capture-verified-full-tests.log`; production validation was unchanged.
- Final all-feature shared/policy/pipeline selection passed **604/604**, 3,118 outside selection, 21.213s (`/tmp/bitrouter-p1-capture-repaired-full-tests.log`). Final minimal shared selection passed **139/139**, 2,291 outside selection, 14.313s; the PostgreSQL campaign took 14.227s (`final-minimal-tests.log`). Local no-default policy/pipeline selection passed **379/379**, 1,884 outside selection, 2.192s (`final-local-tests.log`). All ran at two test threads, with no failed, leaked or retried test status. Each log retains its compilation output.
- Final all-feature library/test Clippy passed in 1m11s with no Clippy diagnostics and **50 staged unused-code warnings** (`final-clippy.log`). Strict no-default local library/binary Clippy passed in 18.22s (`final-local-clippy.log`); formatting and diff checks passed. Rust 1.97.0, debug info/incremental disabled, two build jobs, and the dedicated PostgreSQL 17 database `bitrouter_p1_eval_recheck_20261021` were used. The existing linker frame-size and dependency future-incompatibility notices remain in logs. This is focused slice evidence, not a workspace gate, stock executable or performance result.
- Candidate source/log fingerprints and commands are recorded in `/tmp/bitrouter-p1-capture-candidate.json`. Final bounded independent review approved this capture slice with no open source/evidence findings (`/tmp/bitrouter-p1-capture-review.md`). Receiver integration must also seal original inputs in the operator-resolution transaction before consuming Resolved events; this slice does not yet consume them. Receiver, guard, writer and worker integration plus all P1-A01–A09 gates remain open; P0 disposition is unchanged.
