# Optional receipt latency observation

This implementation records existing receipt custody through the application's
existing OpenTelemetry meter/provider/reader. It is opt-in instrumentation, not
an M5 performance result, a dispatch capability, or a stronger durability claim.
It introduces no receipt field, management endpoint, worker, database schema,
authority, or public SDK type.

## Configuration and export

Use a local tenancy profile with an explicit static OTLP configuration:

```yaml
plugins:
  bitrouter-telemetry:
    otel:
      endpoint: http://127.0.0.1:4318
      service_name: bitrouter-m5
      resource_attributes:
        service.instance.id: one-fresh-measurement-arm
      sampler: always_off
      content_capture: off
      metrics:
        enabled: true
        export_interval_ms: 86400000
    receipt_latency:
      enabled: true
      correlation_id: measured-cohort
```

The private receipt_latency block rejects unknown fields. Its optional exact
correlation selector is bounded to128 bytes and matched against the existing
admitted execution binding; it neither grants access nor changes a receipt.
Omit it to observe every admitted receipt. Warmup may use another real binding.
No selector, execution/tenant/principal ID, resource, credential, provider,
model, URI, raw error, or SQL text becomes a receipt metric dimension.

Absent or disabled observation creates no timing state, instruments, additional
per-owner heap allocation, or clock reads on the receipt path. Ordinary telemetry
retains its best-effort policy. Explicit enabled observation requires local
tenancy, an explicit otel block, an OTLP transport, enabled metrics, and successful
exporter initialization before the Service is published. It rejects the separate
telemetry convenience block rather than selecting an account attribution path.
OTel HTTP wins when both transports are built; gRPC-only uses the same metrics.
No-OTel builds reject explicit enabled observation and produce no p99 result.

The existing collector and final SDK flush are the export consumer. No new
histogram-to-file CLI, Prometheus endpoint, or metrics worker exists. Set the
existing OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=cumulative and verify
actual exported temporality. Pin effective OTEL environment, static headers,
features, resource identity and endpoint. Existing environment overrides remain.
For after-run export, complete the entire bounded arm before the configured
periodic interval. Any periodic export during that window invalidates that
controlled after-run condition; it does not invalidate the application's receipts.

Normal host shutdown first drives the existing Service/host/pipeline cleanup,
then seals observation completeness, then invokes the existing observe shutdown.
Only explicitly enabled receipt measurement returns a local error for an
unclean observation seal or a missing/unclean cached SDK shutdown report. There
is no second flush to replace a failed result. A clean seal is a producer-count
statement, not delivery acknowledgement: independently require collector
acceptance of the final nonzero cumulative data and clean SDK shutdown. A
collector's partial rejection or a missing final packet cannot qualify a run.
The observe status command remains configuration/readiness information, not a
sample-completeness or p99 report.

## Recorded boundaries

- First Open-to-Queued transition saves the monotonic std::time::Instant once,
  before enqueue. Reconciliation retains this historical T0.
- First dequeue includes the actual completion lane wait, even if H refuses an
  already expired queued job before first polling its work.
- prepare includes the real journal settlement/candidate preparation await.
- transaction begins immediately before H BEGIN and ends after its acknowledged
  COMMIT/ROLLBACK or unconfirmed consuming outcome.
- sql_commit starts after the existing Committing phase entry and ends when the
  original consuming SQL await returns. The phase gate is outside that span.
  The original H disposition is published before numeric metrics are emitted.
- queue_ready_to_durable ends after receipt accounting confirmation and the
  existing Durable transition, before waiters are notified. Numeric emission is
  outside the Owner phase mutex. Joining/repeated publication does not count twice.

Commit means the current driver returned Ok under the database's existing
settings. It does not measure physical device fsync completion or prove power
loss durability. Scheduling delay until the await returns is included. The
caller output handshake and final HTTP DONE acknowledgement remain independent.
Late actual durability can be observed after a caller fails, without changing
that failure or sending another provider attempt.

A failed/dropped consuming span is unconfirmed; it never emits an acknowledged
COMMIT or successful durable sample. A known rollback can reconcile the same
immutable obligation; both attempts remain visible and the original T0 remains.
SQL acknowledgement followed by publication failure leaves a count gap. An
Unknown execution receipt is evidence_unknown, never complete_completed.

## Finite schema and validity

The existing observe schema adds five instruments (including the additive
`Instrument::Gauge` case in the public observability schema):

| Name | Type/unit | Fixed dimensions |
| --- | --- | --- |
| bitrouter.receipt.queue_ready_to_durable | u64 histogram, ns | receipt_class: complete_completed, terminal_other, evidence_unknown |
| bitrouter.receipt.phase.duration | u64 histogram, ns | phase: first_queue, prepare, transaction, sql_commit; result: observed, acknowledged, failed, unconfirmed |
| bitrouter.receipt.observation | u64 counter,1 | fixed internal event enum, plus sample_limit/sample_limit_saturated |
| bitrouter.receipt.capacity.bytes | u64 gauge, By | field: completion_reserved, accounted, ingress_reserved; checkpoint: initial, current, final |
| bitrouter.receipt.capacity.state | u64 gauge,1 | field: potential_unknown, live_unknown, active, queued, ingress, closed, fenced, available; checkpoint: initial, current, final |

Histograms use512 finite upper edges: 0 to50ms inclusive in100microsecond steps,
then75ms,100ms,250ms,500ms,1s,2s,5s,10s,30s,60s,120s. The SDK adds one +Inf bucket.
The20ms edge is exact. Values are unrounded integral nanoseconds. Durations above
120s and reversed/unrepresentable clock differences are refused and invalidate
measurement; they are never clipped into a passing bucket.

Each observer has a100,000,000 numeric-update limit. Durations at most120e9ns
bound any one histogram sum below12e18, less than u64::MAX. Once the limit is
exhausted, stop numerical updates, latch invalidity, and retain a bounded rejected
update count and sticky saturation bit. Three reserved final counter writes
report those diagnostics and the seal exactly once; they never feed u64::MAX to
the SDK. State and possible metric series are constant-sized. First SDK series
allocation is real overhead and is not hidden by fabricated zero samples.

A clean seal requires successful producer shutdown, at least one selected
admitted owner, admitted=first_queued=durable counts, a readable quiescent
capacity seal, and no known observer
invalidity. This is necessary, not sufficient, for performance acceptance.
The external artifact verifier must also reconcile the actual authenticated
receipt pages, expected cohort size, all outcome classes and retry/caller-failure
counters; reject unknown/unclassified evidence, count gaps, saturation, malformed
boundaries, noncumulative data, otel.metric.overflow or +Inf samples.

Compute nearest-rank p99 using ceil(99*N/100) for N>0 and integer bucket counts.
Report the containing bucket interval and use its upper edge for a conservative
20ms decision. Do not interpolate, average round p99s, use histogram means, or sum
duplicate cumulative flush/shutdown snapshots. Retain all rounds and deduplicate
identical cumulative snapshots without hiding reset/conflicting series. At20ms
the uncertainty interval is at most100microseconds.

## Capacity snapshots

Capacity is process-wide even when the timing correlation selector excludes
warmup or another tenant. The same provider registers two bounded callbacks only
when observation is enabled. They copy under admission try_lock, release that
guard before SDK observation, and retain only numeric snapshots and a Weak source.
They do not expire queues, wake waiters, recover poison, probe disk, issue SQL,
add per-request work, or change capacity/admission decisions.

Initial is the Worker's actual post-bootstrap/full-H-acknowledgement state,
copied after receipt accounting settles and carried in the initialization ACK
before maintenance starts. Service binds that immutable copy without rereading
Admission; a maintenance lock after the ACK cannot refuse otherwise valid
startup. Genuine bind refusal still awaits the existing Service shutdown. Each callback emits
initial plus current until it observes the first seal, then initial plus frozen
final. Current callbacks are independent: contention omits current byte fields,
or emits current available=0 without other current state fields. available=1 on
the state callback cannot certify the independently collected byte fields.
A collection straddling seal may legally contain current/final subsets.

Final requires real producer cleanup, closed admission and zero active/queued
work, ingress count/bytes and completion reservations. A failed read or unclean
shutdown freezes unavailable and fails the measurement seal. Repeated shutdown
never replaces it with a later better state. Actual retained Unknown/accounted
values are preserved, including escalated Unknown obligations. They are not
zeroed merely because ordinary producers stopped. Gauges cannot retain a file
lease after the actual Service owners exit.

completion_reserved combines 4MiB per unfinished execution receipt with exact
stopped-native repair candidate bytes plus16KiB per missing row. Repair consumes
possible-Unknown room and the same shared byte limits without an active execution
slot. Under Admission's existing mutex the source checks execution/repair counts,
checked multiplication/addition and exact agreement with the aggregate reservation;
it does not infer repair charges from active slots. potential_unknown includes
both execution and repair obligations. No repair charge is omitted or reset for
observation, including unacknowledged consuming-COMMIT custody.
live_unknown includes escalated durable obligations. accounted includes the
existing durable primary, auxiliary and OAuth policy charges. It is neither
heap, physical DB/WAL usage, unknown-only bytes nor a per-tenant quota.
ingress_reserved is the existing body reservation (16MiB for unknown length),
not actor queue bytes. There is no invented actor queue byte scalar.

The finite ceiling is19 duration identities +18 event identities +33 gauge
identities =70 receipt series; histograms retain512 boundaries. There are at most
22 gauge points per collection. No identity labels, new endpoint, runtime crate,
worker or config switch are added. Disabled/no-OTel paths do not bind or snapshot.

The external bounded receiver treats valid partial batches as incomplete and
accepts them without poisoning observation. It requires one complete initial/final
vector in one process/meter batch before the existing final reconciliation
boundary. It never stitches partial exports or current fields. Immutable conflicts,
final unavailable, malformed types/units/attributes/flags/ranges, timing seal and
export failures remain fatal. Gauges are not monotonic cumulative counters.
The static reference compares all actual warmup/measurement receipt encodings,
started/HTTP200 disposition bytes and16KiB row charges against observed accounted
delta. Other accounting profiles require their own exact projection; absence is
never zero. A complete capacity vector alone is not performance acceptance.

## Qualification

The source includes bounded real HTTP/gRPC OTLP receiver tests plus authenticated
Service/provider/SQLite receipt lifecycle tests, arithmetic and schema checks.
They are functional checks; their artificial gate durations are not benchmarks.
M5 requires independent review and actual feature/OS qualification, five paired
workload distributions, receipt/cohort/export reconciliation, and observer-on/off
CPU/allocation/RSS/latency evidence at ordinary and saturated concurrency. Include
first-series and final-export peaks. Fixed cardinality alone is not measured RSS.
The off arm has no receipt p99 and cannot substitute for an instrumented arm.
No performance, cross-instance, strict-receipt, physical-fsync or completion claim
is made by this document.
