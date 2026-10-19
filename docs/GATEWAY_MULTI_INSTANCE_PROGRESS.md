# Native gateway multi-instance progress

Implementation follows the [P1 contract](GATEWAY_MULTI_INSTANCE_SPEC.md) and the
[original P1 gates](BITROUTER_SERVICE_REFACTOR_PLAN.md#p1-acceptance).
Status: **in progress; no P1 delivery gate is accepted yet**.

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
| 3 Gateway integration | Pending | Pending | Pending |
| 4 Published authority | Immutable publication implemented; operator and runtime wiring pending | Real PostgreSQL publication and local parser regressions passed | Publication substage approved |
| 5 Trajectory authority | Pending | Pending | Pending |
| 6 Delivery and operations | Pending | Pending | Pending |
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
