# Tenant service completion

Implementation baseline: PR #1008, commit
`9ea00a13cbf5aaa85f557e2aec3933f673408f9e`. This document fixes the ownership
boundary and acceptance requirements; a requirement is not complete merely
because a DTO, endpoint, or passing unit test exists.

## Ownership

| Component | Owns | Excludes |
| --- | --- | --- |
| `bitrouter-ai` | Validated public request/response types, explicit stream endings, bounded wire encoding, optional default-off Axum integration | HTTP clients, storage, service lifecycle hooks, routing execution, commercial policy |
| `bitrouter-sdk` | Pooled HTTP/1.1 and optional HTTP/2 clients, per-call scoped credentials and capability bindings, bounded JSON/SSE decoding, protected management and evidence reads | Automatic inference replay, authority discovery as authorization, application accounting, Axum production dependencies |
| `bitrouter` | Generic resource authority, immutable configuration, encrypted provider credentials, routing and inference scheduling, actual execution evidence, durable receipts and recovery | External user accounts, subscription/credit/price rules, payment and financial settlement, application-specific identity rewriting |
| Service consumer | External identity and business rules, public API compatibility, durable business-to-execution mapping, reconciliation and financial decisions | Forging tenant/principal headers or treating missing evidence as zero |

The deployment target is the existing opt-in Unix, exclusively owned SQLite
single process. Distributed tenant authority, another transport such as gRPC,
plugin ABIs, remote lifecycle hooks, and an additional agent execution runtime
are outside this change. Existing independent gateway operation and feature
cuts remain supported.

## Required changes and acceptance

1. **Scoped SDK dispatch.** Use the existing `DispatchBinding` and a per-call
   tenant Bearer credential on the same connection pool. Cover Chat, Messages,
   Responses JSON/SSE and Decisions JSON. Validate all seven binding headers;
   no arbitrary identity/control header override. Preserve execution identity
   where the server supplies it and explicit unknown where it does not. Do not
   resend on timeout, cancellation, malformed response, stale discovery or
   protocol mismatch. Test concurrent credential isolation, revoked/stale
   capabilities, HTTP/1.1 and h2c, errors and stream termination.
2. **Execution lifetime.** Separate bounded request admission/upload from
   execution and idle limits. A healthy long stream must not inherit the old
   hardcoded 60-second admission deadline. Configured expiry must cancel work,
   retain evidence and preserve strict receipt-before-success ordering; it
   must not start another execution. Test long progress, actual expiry,
   cancellation, shutdown and invalid duration configuration.
3. **Resource lifecycle.** Add protected, versioned generic resource
   provisioning/status/revocation for upstream credential slots, routes and
   policy definitions. Reuse the authority actor, encrypted immutable store,
   ownership scopes and operation lookup. Mutations require installation
   binding, stable operation identity and explicit CAS. Secret material is
   write-only and excluded from replies, diagnostics and operation storage.
   Endpoint authority must be explicit: untrusted tenant requests cannot turn
   resource drafts into arbitrary access to the host network. Rotation creates
   a new immutable revision; activation is explicit and old revision
   revocation fences later dispatch even through caches. Verify concurrent
   CAS, replay/conflict, permissions, pool isolation, lost replies, restart,
   key mismatch, storage failure, atomic publication and retained revocation.
4. **Technical evidence.** Add a versioned, bounded, content-free evidence
   contract without changing the strict v1 receipt schema. Capture actual
   attempt identity, native numeric usage observations in order, source and
   snapshot/delta semantics, web-search/media counts when reported, and
   estimator algorithm/version when used. Missing, zero and estimated values
   remain distinct. Unsupported or truncated evidence is explicit. Preserve
   evidence for fallback, provider errors and client disconnects, under the
   same durable terminal gate. No prompts, tool arguments/results, secrets or
   prices are retained in this evidence. Test adversarial provider data,
   repeated usage frames, failed/partial streams, nested attempts and recovery.
5. **Routing and operational qualification.** Reconcile generic capability
   filtering, provider/model RPM, metric ordering, circuit health and bounded
   fallback with the actual retained execution path. Close missing generic
   behavior and test it through authorized resources rather than importing
   application business rules. Verify admission/backpressure, storage bounds,
   unknown outcome capacity, scoped reads and crash recovery in the final
   single-instance composition. Do not infer hosted throughput or distributed
   safety from local tests.

## Delivery gates

- Independent review at each stage, and a fresh final adversarial reviewer.
- Contract schemas/examples, SDK API baselines and operator documentation
  updated with their implementation. No mandatory runtime/client dependency
  added to AI and no production Axum dependency added to SDK.
- Focused behavioral tests followed by relevant feature combinations, full
  unit/integration/doc tests, formatting and Clippy. Actual process tests must
  exercise SDK-to-service boundaries; mocks alone do not establish service
  acceptance.
- A new PR above #1008. That base has now merged as
  `80550c9b2079a6e23ceeeb6babf922a77e99b5bb`; delivery targets `main`, including
  its separate CI fixes. Record source commits and actual verification results.
  No production rollout, migration of consumer databases, or performance
  qualification is implied by opening the PR.

## Evidence ledger

All five implementation slices are present. Separate source reviewers approved
the SDK, resource authority/storage, execution lifetime, routing, usage evidence,
SQLite migration atomicity and actual-process test stages after corrections.
A fresh whole-change reviewer also approved the integrated source and subsequent
recovery, custody, capacity and fixture corrections, including the test fixture's
MSRV-compatible atomic reservation. Source approval and execution evidence are
recorded separately below.

The reviews resulted in the following substantive corrections:

- Persist rejected resource CAS outcomes so lookup and replay remain stable
  across restart; never store an unkeyed credential command fingerprint.
- Keep output cancellation cleanup separate from receipt settlement. Preserve
  the original finalizer/ACK waiter across a bounded drain timeout, and release
  capacity only after confirmed completion.
- Classify circuit availability only from real upstream HTTP status, including
  synthetic-502 regression coverage. Read Responses throughput usage from the
  actual terminal event and preserve exact resolved policy-tier ordering.
- Require eligible matching SSE labels before recording native usage. Persist
  capture provenance in new recovery seeds, distinguish lost evidence from
  historical absence, and bound original SDK response bytes before decoding.
- Commit SQLite schema changes, validation and migration ledger together;
  failure leaves the preceding schema intact without adopting unknown tables.
- Exercise concurrent tenant calls over a shared SDK pool on HTTP/1.1 and h2c,
  exact native frame order, restart retention, active credential revocation and
  transport timeouts through actual service processes.
- Retain stopped-owner recovery for both the historical preparation prefix and
  the new routing-custody prefix. Both require exact identities, ordering and
  complete successful ACKs; unfinished or foreign work remains ineligible.
- Keep cancellation tests attached to actual producer/commit custody and derive
  storage-capacity witnesses independently from receipt and sidecar bytes.
  Bound the native experiment fixture at 160 KiB while proving that its former
  128 KiB checkpoint budget refuses before any provider dispatch.
- Charge bounded usage-evidence reads from SQL-measured UTF-8 lengths before
  transferring payloads. Retain one shared 32 MiB operation budget, absolute
  SQL cell bounds and measured-length guards against intervening growth. The
  earlier per-record maximum reservation incorrectly exhausted this budget
  during a 128-record cohort read.

Public API dependency manifests remain unchanged. The default AI public API
contains no optional HTTP/runtime types; SDK public dependencies remain
`bitrouter_ai`, `futures_core`, `http` and `reqwest`. Formatting, added-source
boundary checks and local specification links have passed.

Completed local validation on macOS arm64 with Rust 1.99:

- AI and SDK all-feature unit/integration tests and doctests passed, including
  scoped dispatch and original-byte evidence bounds. Their all-feature Clippy
  checks passed with warnings denied.
- The complete workspace Clippy check passed after the final reviewed
  corrections, using `--workspace --all-features --tests -- -D warnings`.
- The complete all-feature workspace suite passed: 5,878 tests, zero failures,
  338 skipped. Three expected long-running cases were reported as slow; no
  timeout, retry or output-pipe leak was reported in this run.
- All 85 focused recovery, cancellation, native experiment and capacity
  regressions passed. The separately selected ignored stopped-owner campaign
  passed all eight actual subprocess scenarios, preserving repair and
  no-resend assertions.
- Minimal gateway (`--no-default-features`) tenancy, configuration and SQLite
  migration tests passed: 506 selected tests, zero failures. One pure
  configuration case reported an output-pipe leak warning; its all-feature
  rerun passed without that warning or any runner-limit change.
- All five actual SDK-to-service process tests passed in the final full suite:
  HTTP/1.1 and h2c across seven protocol modes, concurrent tenant isolation,
  credential rotation/revocation and restart, dropped SSE and client read
  timeout. The successful Chat fixture uses consistent usage snapshots; the
  original contradictory snapshots correctly failed strict receipt assertions.
- The complete configuration schema was regenerated from the default product.
  Public API foreign-type baselines and default AI isolation passed.
- Complete workspace all-feature doctests passed: 13 passed and one explicitly
  ignored. Final formatting and diff whitespace checks passed.

These local runs cover candidate `4b71915` plus the final reviewed recovery and
fixture corrections. The delivery rebase also incorporates upstream CI coverage
and registry updates (#1012 and #1014); PR CI validates the resulting integrated
head. Hosted throughput and distributed authority remain unqualified.

CI follow-up on `6b8f4df` found two additional issues in explicitly selected
process/capacity campaigns: recovery witnesses omitted usage-sidecar storage,
and repeated fixed-maximum evidence reservations rejected the final cohort
read after all 128 executions had reconciled. Independent review approved the
corrected storage witnesses, actual-length reader and five adversarial reader
regressions. Rebuilt runtime verification passed:

- All five reader regressions, including 128 small sidecars on a shared budget
  with 24 MiB already consumed, refusal before payload transfer, and growth or
  corruption between metadata and payload reads.
- All 127 related receipt and recovery tests, including the original 60-second
  HTTP/1.1 and HTTP/2 ingress deadlines.
- All 14 paid-recovery parent campaigns, the six-mode repair-observation parent
  and the eight-mode stopped-owner parent.
- The unchanged 128-execution mixed-arm campaign, including final cohort
  reconciliation, completed in 70.17 seconds with the original limits.

The follow-up workspace Clippy passed with warnings denied, and final formatting
and diff checks passed. The committed candidate must also pass the complete PR
CI gate.

The unchanged PostgreSQL slow-reader fixture also reported a transient socket
pressure failure; an unchanged rerun passed the entire PostgreSQL job, including
that test. The fixture and shared-gateway production path were not modified for
this failure. All other first-candidate CI checks passed, including Windows,
macOS, Linux ordinary tests, MySQL, feature closures, MSRV, public API boundaries,
Clippy, docs and distribution checks.
