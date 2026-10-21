# Shared gateway acceptance

Candidate for [P1](BITROUTER_SERVICE_REFACTOR_PLAN.md#p1-acceptance), stacked on
[#984](https://github.com/bitrouter/bitrouter/pull/984) in
[#990](https://github.com/bitrouter/bitrouter/pull/990).
Status: P1 implementation and local acceptance complete (2026-10-21).
Hosted checks and review state are tracked on the stacked PR.

The [implementation contract](GATEWAY_MULTI_INSTANCE_SPEC.md) defines the
supported behavior. The [progress record](GATEWAY_MULTI_INSTANCE_PROGRESS.md)
retains historical failures, repairs and intermediate evidence. This document
identifies the candidate and aggregate results. Hosted checks remain visible on
the PR; earlier failed runs are retained in the progress record.

## Acceptance map

| Gate | Executable evidence | Final status |
| --- | --- | --- |
| A01 Shared admission | Two stock replicas contend for one tenant limit and one provider-account pool through two aliases; unrelated tenants/accounts still progress. | PASS: final aggregate and eight fixed stability sets |
| A02 Idempotency | Cross-replica duplicate/conflict submissions and an ambiguous provider response preserve one execution and exact provider counts. | PASS: final aggregate and eight fixed stability sets |
| A03 Crash recovery | Owned SIGKILL at Accepted, Prepared, in-flight inference, delivered stream delta, and attempt-terminal/execution-terminal boundaries; survivor status and retained/released capacity are checked. | PASS: final aggregate |
| A04 Finalization | PostgreSQL competing terminal transactions and stock outbox replay after a proven ACK abort preserve original metering/projection effects and exact dispatch counts. | PASS: final aggregate; eight fixed outbox stability sets |
| A05 Shared authority | Operator principal/resource/credential/config changes, cross-replica native continuation, tenant isolation, invalid authority and actual database TCP loss/recovery. | PASS: final aggregate, OAuth cancellation regressions and three maintenance deadline repeats |
| A06 Trajectory guards | Alternating/concurrent requests, history completeness, counter/cost thresholds, active holds and retained key epochs; real PostgreSQL catch-up and original-capture regressions. | PASS: final aggregate, stale-snapshot red/green regression and eight fixed stability sets |
| A07 Bounded load | Configured connection/body bounds, slow ingress, unrelated tenant/replica progress, and assembled-service response backpressure. | PASS: final aggregate, including stock stalled-reader backpressure/resume/interruption |
| A08 Rolling shutdown | Held-stream graceful completion, replacement at the drained address, second-signal and absolute-deadline interruption with truthful terminal state. | PASS: final aggregate |
| A09 Local regression | Default/full and minimum SQLite builds, independent dependency closures and controlled P0/P1 process measurements without PostgreSQL running. | PASS with recorded footprint/SSE costs and follow-ups; initial and fixed-repeat samples retained |

## Candidate verification

Runtime source: `2495bf8db118de399a11fc11f82ba18bd6a6455d`, based on P0
`9cbe512d101e777470d113b0086ab9e331b3a638`. The immutable v9 `gateway-shared`
executable has SHA-256
`f6ca2bf8820d81c96c4337c609554fa7371f6e88c2b1f85a70bf9b97ff8efb6c`.
[Artifact identities](evidence/gateway-multi-instance/artifacts-2026-10-21.json)
record compiler, features, sizes and local comparison binaries.

The final shared selection passed **203/203** in **93.149s**, with two test
threads and zero retries. It includes both ignored PostgreSQL campaigns and
ordinary shared regressions. The fixed stability campaign passed **40/40**
across eight five-test sets on v8; the subsequent repair changes only the native
maintenance deadline and an equivalent Store deadline extraction. That repair
passed the final v9 aggregate and **3/3** additional fixed focused runs. Strict
workspace Clippy passed on the final runtime source. The full workspace passed
**4,939/4,939** in **677.839s**, with 147 configured skips; doctests passed **11**
with one ignored. Warning-denied rustdoc, registry, generated distribution and
format checks passed. [Validation evidence](evidence/gateway-multi-instance/validation-2026-10-21.json)
records commands, counts and log identities. One non-reproduced nextest output
handle notice in an unchanged P0 test is retained with its scoped review and
single zero-retry check in the [evidence notes](evidence/gateway-multi-instance/README.md).

Independent review closed the shared OAuth ownership defect, the stale guard
snapshot race, the maintenance deadline/cursor defect, and the foundation
fixture stack issue. Their failing evidence and scoped repairs remain in the
progress record; test retries and increased timeout/stack budgets did not hide
those failures.

Ordinary workspace tests exclude opt-in external-adapter campaigns. Shared
PostgreSQL validation has two explicit selections: the stock process family and
the non-process shared authority/native-continuation/writer-isolation families.
Neither uses automatic test retries. The packaged Linux job consumes the actual
`gateway-shared` archive and executes both selections against PostgreSQL 17.

Local performance compares identical Rust compiler, target and build settings.
Development-build results describe that controlled workload; they are not a
production throughput or multi-replica scaling claim. The accepted P0 footprint
overhead remains the baseline, not permission to introduce an unmeasured P1
overhead. The existing Rust measurement fixture checks response content, complete
SSE framing, exact provider counts and owned child shutdown for every sample.

The [local comparison](evidence/gateway-multi-instance/README.md) retains all
27 samples and 221,616 provider calls. Normal/build dependency trees and features
are unchanged. Default idle RSS increased 0.406 MiB in the initial block and
0.797 MiB in the fixed repeat; minimum idle increased 0.813 MiB. Executables grew
0.316% and 0.976%, respectively. Default SSE median throughput was 1.13% and
2.33% lower; the initial large SSE slowdown did not recur. Default after-load
RSS medians changed +5.57% and -1.87%, with overlapping ranges. A09 accepts these
recorded costs for this candidate, retaining footprint/SSE optimization and
unreplicated-signal follow-ups. It does not establish a universal numeric budget
or claim zero overhead, unchanged tails, or absence of a memory leak.

## Delivery limits

- Shared mode is explicit and opt-in. It requires PostgreSQL, authentication,
  namespace bootstrap and a common deployment key. Local SQLite remains the
  default and does not acquire a shared coordinator.
- Live streams remain attached to their original process. Unknown provider work
  stays uncertain and capacity remains held until trustworthy terminal evidence
  or explicit operator resolution; lease expiry does not establish cancellation.
- Terminal outbox delivery may repeat. Stable identity and idempotent projection
  prevent duplicate local effects; external consumers must deduplicate.
- Replicas in one namespace use the same build/private storage format. This
  candidate does not establish mixed-version rolling upgrades or automatic
  storage rollback.
- The scope covers gateway resource scheduling and authority. Shared Core or
  harness execution, tool replay, business billing and external-provider
  performance are outside this acceptance.
