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
| 3 Gateway integration | Credential binding, admission, dispatch, delivery and private HTTP router implemented; official startup pending | Scoped HTTP/protocol/executor and PostgreSQL regressions passed | Authentication/context/dispatch/custody/HTTP substages approved |
| 4 Published authority | Immutable publication implemented; operator and runtime wiring pending | Real PostgreSQL publication and local parser regressions passed | Publication substage approved |
| 5 Trajectory authority | Pending | Pending | Pending |
| 6 Delivery and operations | Shared submission/query SDK, bounded custody/transport/HTTP implemented; runtime/workers/operator pending | Scoped service regressions and independent SDK tests passed | Custody/transport/HTTP/SDK substages approved |
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
