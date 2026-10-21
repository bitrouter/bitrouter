# Shared gateway acceptance

Candidate for [P1](BITROUTER_SERVICE_REFACTOR_PLAN.md#p1-acceptance), stacked on
[#984](https://github.com/bitrouter/bitrouter/pull/984) in
[#990](https://github.com/bitrouter/bitrouter/pull/990).
Status: final verification in progress (2026-10-21).

The [implementation contract](GATEWAY_MULTI_INSTANCE_SPEC.md) defines the
supported behavior. The [progress record](GATEWAY_MULTI_INSTANCE_PROGRESS.md)
retains historical failures, repairs and intermediate evidence. This document
will identify the final candidate and the aggregate results; intermediate
passes below do not establish final acceptance.

## Acceptance map

| Gate | Executable evidence | Final status |
| --- | --- | --- |
| A01 Shared admission | Two stock replicas contend for one tenant limit and one provider-account pool through two aliases; unrelated tenants/accounts still progress. | Aggregate and bounded stability run pending |
| A02 Idempotency | Cross-replica duplicate/conflict submissions and an ambiguous provider response preserve one execution and exact provider counts. | Final aggregate pending |
| A03 Crash recovery | Owned SIGKILL at Accepted, Prepared, in-flight inference, delivered stream delta, and attempt-terminal/execution-terminal boundaries; survivor status and retained/released capacity are checked. | Final aggregate pending |
| A04 Finalization | PostgreSQL competing terminal transactions and stock outbox replay after a proven ACK abort preserve original metering/projection effects and exact dispatch counts. | Final aggregate pending |
| A05 Shared authority | Operator principal/resource/credential/config changes, cross-replica native continuation, tenant isolation, invalid authority and actual database TCP loss/recovery. | Final aggregate and OAuth cancellation repair pending |
| A06 Trajectory guards | Alternating/concurrent requests, history completeness, counter/cost thresholds, active holds and retained key epochs; real PostgreSQL catch-up and original-capture regressions. | Final aggregate pending |
| A07 Bounded load | Configured connection/body bounds, slow ingress, unrelated tenant/replica progress, and assembled-service response backpressure. | Stalled response-reader campaign pending |
| A08 Rolling shutdown | Held-stream graceful completion, replacement at the drained address, second-signal and absolute-deadline interruption with truthful terminal state. | Final aggregate pending |
| A09 Local regression | Default/full and minimum SQLite builds, independent dependency closures and controlled P0/P1 process measurements without PostgreSQL running. | Final comparison pending |

## Candidate verification

The parent owns Cargo, database fixtures and application processes; independent
reviewers inspect fixed source snapshots and draft patches separately. The final
record must include the source revision, compiler/profile/features, immutable
binary hashes, exact test selections and retained failure dispositions.

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
