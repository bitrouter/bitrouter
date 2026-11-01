# Tenant service implementation

This guide describes the application-owned tenant service and its client
boundary. The [service API](../specs/service-api/README.md#tenancy-api) is the
canonical wire contract; the [configuration specification](../specs/configuration/README.md)
and generated schema define host settings. This guide evolves with the runtime.

## Deployment and ownership

Tenancy is an explicit opt-in to a single Unix process holding an exclusive
lease on a file-backed SQLite database. It requires authenticated inference, an
enabled control listener and an explicit `control:tenancy` operator credential.
Shared gateway mode and the host coding-task HTTP API are rejected in this
profile. Startup validates the configuration and obtains ownership before
opening listeners; decoding public types does not enable these capabilities.
The [configuration validator](../../apps/bitrouter/src/config/tenancy.rs) and
[file owner](../../apps/bitrouter/src/tenancy/ownership.rs) enforce eligibility.
This profile does not establish distributed tenant authority or hosted capacity.

| Component | Responsibility |
| --- | --- |
| `bitrouter-ai` | Public types, intrinsic validation, explicit stream endings and bounded framing; optional default-off Axum integration |
| `bitrouter-sdk` | Pooled HTTP transport, caller-scoped dispatch, bounded JSON/SSE decoding, protected management and evidence reads |
| `bitrouter` | Authentication and resource authority, encrypted immutable configuration, routing, execution custody, technical evidence and durable recovery |
| Consumer service | External identity, business roles, prices, credits, payments, public API compatibility and reconciliation against execution identities |

AI opens no client or listener and owns no storage or lifecycle hooks. SDK owns
no routing executor, application accounting or production Axum dependency.
Consumers compose their own handlers and middleware with the optional AI Axum
binding; the [SDK example](../../crates/bitrouter-sdk/examples/axum_gateway.rs)
shows that composition. A framework adapter's protocol end does not itself prove
that the service committed an execution receipt.

## Resource publication

The [resource authority](../../apps/bitrouter/src/tenancy/resources/management.rs)
serializes credential, route and policy publication through the existing authority
actor. Secrets are encrypted in immutable, installation-bound revisions, with
keyed command fingerprints and nonsecret status projections. Configuration
activation explicitly selects revisions; rotating a credential does not retarget
an existing configuration. Revocation fences subsequent compilation, activation
and sends, including cached routes, while retaining earlier execution evidence.

Tenant-authored routes require operator-approved exact origins and restricted
headers. An empty origin list keeps route provisioning operator-only. The
operator owns origin approval and DNS/network reachability. Removing an origin
from startup configuration does not revoke a retained resource; withdraw its
revision explicitly. The [operator guide](../../skills/bitrouter/references/remote-administration.md)
describes setup and management operations; permission, CAS and replay behavior
are defined in the canonical service API.

## Client dispatch

A `GatewayClient` owns the shared connection pool. Its
`tenant(bearer, &binding)` helper binds one credential and all seven validated
capability headers per call, without changing the pool's default credential.
Chat Completions, Messages and Responses support JSON/SSE; Decisions uses JSON.
The transport can select HTTP/1.1 or feature-gated HTTP/2 prior knowledge. It
does not probe another protocol or replay inference after an uncertain result.

Discovery describes available facilities; live service authority is still
checked for each attempt. Keep admitted execution identity separately from
caller correlation. A trusted `not_started` refusal is different from unknown
delivery. Query the original execution with current authority after a lost
response; a missing receipt is not proof that provider work never ran.
Management writes instead recover by their original stable operation ID.
See the [SDK usage guide](../../crates/bitrouter-sdk/README.md) for concrete types,
transport limits and response metadata.

## Execution ownership

An admitted invocation receives a fresh execution identity and one receipt owner
in the existing execution registry. The owner binds the installation, tenant,
principal, correlation and immutable configuration revision. Reusing a correlation
identifier creates another execution; it never deduplicates inference.

The ordinary admission mutex reserves execution capacity and completion storage
together. A disconnected HTTP waiter cannot release either reservation while
provider work, required finalization or durable receipt work remains unsettled.
The existing task tracker owns preparation, stream polling and rendering. Returned
bodies use bounded, revocable handoffs rather than retaining the execution owner.

Each invocation permits at most eight actual model attempts. A private permit is
reserved before upstream request authentication and armed only after the last current-authority
check binds the actual authenticated request. Attempt facts preserve the configured
provider identifier, exact retained target and configured credential slot. They
exclude credential secrets, raw account labels, prompts and model output.

## Provider facts and completion

Usage comes from the original supported provider response, before canonical
conversion or hooks. Missing counters remain unknown; explicit zero is evidence.
Repeated snapshots do not add usage twice. Conflicts, invalid quantities and
unsupported evidence cannot be repaired by canonical defaults. A local output
failure does not erase a provider completion already observed and validated.

The sole authority writer persists an immutable terminal candidate through its
existing transaction custody. Connected JSON success and the final SSE success
signal wait for acknowledged receipt commit. Earlier provisional stream frames,
including protocol finish metadata, do not assert durable completion. The final
delivery signal remains in the caller's actual body poll.

HTTP continuation publication and receipt persistence have separate obligations.
A committed receipt cannot prove that continuation cleanup succeeded. Only the
concrete continuation owner can acknowledge its own compensated preparation;
unknown or failed cleanup remains retained.

An acknowledged rollback may reconcile exactly the same receipt candidate.
An uncertain consuming SQL operation retains writer and execution ownership.
Neither reconciliation, freeing disk space nor an elapsed timer replays inference
or turns an unknown outcome into success. Revoking a caller prevents future sends
and reads while preserving completion of previously admitted obligations.

## Limits and storage

Ingress allows at most 128 uploads and 64 MiB of reserved body bytes, with a
16 MiB ceiling per request and an absolute submission deadline (60 seconds by
default, configured with `tenancy.submission_timeout_secs`). Bodies
without Content-Length reserve the entire per-request ceiling. These reservation
limits are not a claim about total process RSS.

Submission bounds upload, queueing and initial admission. After admission,
`tenancy.execution_timeout_secs` optionally bounds the whole execution; omission
or `null` means no total deadline. A configured total starts when capacity is
admitted, and Choice children inherit their parent's remaining ceiling. A new
native host/session operation gets a fresh submission budget, never renewed
authority for an existing execution. Unattached admissions still expire within
the submission budget. Preparation callbacks can own paid provider work and
therefore follow execution cancellation and its optional total. Per-stream-part
callbacks use the idle budget; storage operations retain their shorter bounds.

`tenancy.stream_idle_timeout_secs` defaults to 120 seconds. It independently
bounds provider reads and a blocked output handoff; each new read or handoff gets
that idle window. Thus active streams can continue beyond 60 seconds, while a
silent upstream or an unread output slot cannot indefinitely hold capacity.
Dropping the preparation waiter cancels its owned provider call, including before
response headers. Already admitted settlement work remains retained until its
actual producers and durable receipt settle. No timer retries provider work.
These are startup-only host settings, not tenant-controlled public contracts.
All configured durations must be positive integer seconds, at most 31,536,000
(one year); zero, overflow and values outside that bound are rejected.

Terminal receipt settlement has a fixed 30-second budget. Cancellation and
shutdown stop new sends, revoke output slots and start a separate, single
30-second producer-join budget, capped by the configured total plus its cleanup
grace. An unread response can outlive settled receipt work without spending its
later cancellation budget in advance. Neither budget renews a transaction or an
already running callback. Unknown provider effects remain unknown, and
successful delivery still requires an acknowledged durable receipt. If actual
producers or storage cannot be joined, authority is fenced and owners are
retained. A later drain may confirm those joins and release capacity against the
same expired deadline; it cannot clear the failure fence, replay work, or infer
cleanup from elapsed time.

Each admitted execution reserves 4 MiB for its bounded receipt and initial
storage allowance. Strict admission stops below 768 MiB of free space or at the
8 GiB receipt high-water mark; 256 MiB is reserved for maintenance. Accounting
includes a conservative 16 KiB row allowance. Physical write amplification and
filesystem exhaustion require separate measurement and fault qualification.

Unknown receipts are bounded to 100 per tenant and 1,000 per instance. They are
escalated for operator investigation after 15 minutes and are not automatically
pruned or reclassified. The 30-second cleanup window is not permission to forget
an uncertain transaction or an unjoined producer.

## Reading evidence

The protected control listener exposes the existing execution and correlation
GET operations. Reads require current tenant-wide receipt authority or a
separately issued audit credential. They verify installation and tenant ownership,
use bounded pagination, and recheck authorization after waits. Responses use
`Cache-Control: no-store`. A missing receipt is never proof that no model call
occurred.

## Native usage evidence

The [capture module](../../apps/bitrouter/src/tenancy/receipts/usage_evidence.rs)
records bounded numeric observations before conversion, with actual attempt
identity, source order and snapshot semantics. It does not retain prompts,
generated text, arbitrary provider keys, tool payloads or secrets. Missing, zero,
invalid, unsupported and truncated observations stay distinct. Estimator
provenance and owned child links cannot substitute for provider-reported totals.
The [usage evidence contract](../specs/service-api/README.md#execution-usage-evidence)
defines the wire interpretation and compatibility cases.

The terminal receipt and its evidence sidecar share one durable publication
gate and are bound by integrity digests. The existing strict receipt shape is
unchanged. Versioned recovery seeds distinguish capture lost during recovery
from historical executions that predate capture; neither state means zero usage.
No read or recovery path repeats inference to repair missing evidence.

The [evidence reader](../../apps/bitrouter/src/tenancy/receipts/usage_evidence/storage.rs)
charges actual bounded SQL-measured UTF-8 sizes before transferring payloads.
It retains a shared operation budget across records, absolute cell limits,
measured-length guards against intervening growth and receipt/evidence integrity
checks. Stored byte-count metadata is not trusted to authorize allocation.

SQLite resource/evidence schema changes and the migration ledger commit
atomically. Startup rejects incompatible catalogs; populated down-migrations are
refused rather than discarding immutable revisions or evidence. Back up the
complete stopped service database and wrapping key before a binary/schema
rollback; changing executables alone is not a supported data downgrade.

## Routing and diagnostics

[Runtime routing](tenant-routing.md) explains the authorized candidate set,
provider/model RPM, metric ordering, circuit ownership and bounded fallback.
These controls remain process-local. Resource, receipt and evidence reads
recheck current scope after waits; telemetry is diagnostic and never substitutes
for the durable receipt or grants execution authority.
