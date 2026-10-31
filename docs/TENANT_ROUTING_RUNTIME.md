# Tenant routing runtime

The hosted tenant pipeline orders only candidates authorized by the retained
route and exact credential resource. It does not add targets, resolve ambient
credentials, or widen the existing protocol and capability projection. Runtime
metadata is derived from authenticated retained configuration without changing
the compiler-v1 target or encrypted envelope format.

## Configuration

Existing router/preset `routing.sort` accepts `alphabetical`, `cost`, `latency`,
and `throughput`. The baseline is the retained authorized order. For tenant
routing, `cost` compares the optional model `routing_weight` as an exact unsigned
integer, ascending. This is a caller-supplied technical relative weight: no
currency, provider tariff, discount, or billing calculation is involved. Published
catalog prices never supply this field. An absent weight sorts after known
weights; ties preserve the retained order.

Provider `rate_limits` reuse the existing longest-match pattern selection. The
matched pattern identifies a shared provider bucket; model
`rate_limits.requests_per_minute` additionally limits the exact model. Both RPM
limits apply atomically. Zero denies sends. Token-per-minute limits are outside
this runtime. Example provider fragment:

```yaml
rate_limits:
  - '*': { requests_per_minute: 120 }
  - 'model-*': { requests_per_minute: 40 }
models:
  - id: model-one
    routing_weight: 12
    rate_limits: { requests_per_minute: 20 }
```

Latency uses average completed attempt duration from actual send. Throughput
uses observed output tokens divided by total elapsed seconds, descending. The
runtime keeps at most 32 samples per exact candidate, requires at least three
fresh samples, and excludes observations after five minutes. Unknown quantities
and estimated usage do not become zero observations. The legacy canonical usage
shape cannot distinguish absent output from explicit zero, so this adapter only
admits positive provider-reported output counts as throughput samples. Latency
can still be observed without a token count. Missing and stale metrics sort last;
missing/missing and equal scores preserve retained order.

## Admission, health, and ownership

One shared runtime belongs to a service instance. Keys include installation,
the complete authorized credential resource reference (owner, kind, ID and
revision), provider, model, account, and the exact endpoint string. Provider
pattern buckets omit only the model. Different tenant-owned credential resources
cannot collide. Grants to the same operator-owned credential intentionally share
its namespace. Credential material never enters these keys or logs.

The actual-send gate reserves both RPM buckets and circuit ownership under one
lock immediately before receipt arming. Failed arming or unsent cancellation
refunds the reservation. Once marked sent, cancellation consumes RPM. Rolling
windows use at most 61 one-second buckets and round expiration upward, so a
request may wait up to one additional second but cannot exceed a rolling
60-second ceiling.

The gate requires the installed request-local routing profile and its exact
admitted execution. A reconstructed context cannot borrow another execution's
profile, even with the same caller and route. Custom tenant pipelines must
install the routing hooks before building. Caller revocation prevents further
sends but does not erase observations from an already admitted attempt. Receipt
completion preserves the original deadline and cancellation ceilings; tenant
inactivity remains a separate continuation-publication fence.

Three explicit upstream availability failures open a circuit for 30 seconds.
Only the executor's observed HTTP-status callback records such failures. Generic
hop or request errors, including synthetic 502 errors for a truncated HTTP 200
body or malformed SSE, do not count as provider availability failures.
After cooldown, exactly one attempt owns a half-open probe. Unsent cancellation
releases ownership immediately; sent cancellation/unknown completion releases it
without claiming success and restarts cooldown. Older in-flight successes cannot
heal a newer circuit generation. Client, authorization, conversion, and local
admission failures do not poison shared provider health.

The runtime bounds state to 2,048 combined provider/target entries and 4,096
outstanding admissions, with at most 16 KiB of identity bytes per candidate.
Capacity exhaustion fails closed. Cleanup never evicts an outstanding owner or
live RPM window; stale idle entries become reclaimable after five minutes.
These are process-local controls, not distributed quotas or durable rate state.
Ordinary sends expire only their two exact buckets. A full bounded cleanup runs
only when inserting new identities would exceed capacity; unrelated tenants do
not add work to the normal admission path.

Receipt custody remains the first fallback gate. The routing profile also denies
fallback for timeouts, invalid responses, cancellation, and other unknown
execution outcomes. Known local RPM/circuit refusals may reach the existing
fallback policy before a receipt is armed. Every subsequent candidate must still
be in the authorized retained set and pass current authority and receipt fences.
An executor-internal OAuth continuation must close the prior routing observation
with its explicit response before another send; the observation callback never
authorizes the continuation itself.

## Validation

The pure scheduling tests cover concurrent RPM, unsent/sent cancellation,
provider/model atomicity, exact credential/account/endpoint isolation, metric
freshness and minimum samples, stable ties, integer weight precision, throughput,
exclusive probes, stale completion generations, and bounded state. Profile tests
cover strict fallback classification. Retained-resource tests verify metadata,
matched-pattern limits, unowned target rejection, and unchanged target encoding.
Additional regressions drive real tenant ingress/provider sockets for repeated
HTTP 200 body/stream failures versus real 503 responses, and drive the actual
StreamProcessor to sample usage carried by Responses terminal events. Exact
resolved-selector lookup keeps a policy tier's sort separate from a coincident
public resource name.
Custody regressions cover missing and replaced execution ownership, unowned
targets, and completion observations after caller revocation while another send
is denied.
