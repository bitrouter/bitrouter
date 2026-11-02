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

## Runtime budgets and indexed authority

`tenancy.capacity` supplies finite startup budgets for active execution, waiting
work, ingress, management, caches, authority views, routing and transport. The
[configuration specification](../specs/configuration/README.md#process-capacity)
defines their fields, units and defaults. Operators can raise them subject to
machine representation, checked allocation and budget relationships. They do
not impose a directory-size ceiling or preallocate one entry per stored tenant.
The exclusive SQLite connection and single authority writer remain unchanged;
ordinary, safety and completion queues rotate service so each can make progress.

Startup validates the entire directory and retained authority through bounded
keyset pages, including all retained operation IDs. Page size and working bytes
bound temporary memory; no successful prefix substitutes for the complete scan.
Format-2 validation does not apply the former directory or lifetime operation
count ceilings. Historical format-1 counter rules remain format checks until
the store is explicitly upgraded. Runtime reads load only the caller and exact
dependency closure under read custody. The authority registry retains a small
publication header and bounded leased views. Committed deltas update those views
under a common publication gate; unrelated mutations do not invalidate a native caller solely
because the public revision advanced. Every final lease check observes the same
service liveness fence. Shutdown closes dispatch before draining HTTP while
retaining the writer for admitted completion and cleanup.

Authority-view bytes reserve one third for leased and optionally retained facts
and two thirds for staged replacements and construction overlap. Fresh submission
ingress may reuse a bounded retained lease after checking its current authority
and exact dependencies; reuse still consumes an admitted read job. Pressure drops
optional retention, without evicting active leases, before retrying allocation.
Expansion is checked before cloning; revocation can publish when the resident
budget is full. Allocation is lazy and does not preload directory entries.
Immutable cache entries and pinned values have separate entry/byte budgets.
These are payload-accounting bounds, not a measurement of total allocator RSS.

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

Operational profile publication uses the same authority writer and immutable
resource store. The protection index is derived from every authenticated
profiled-route revision, including revoked and non-head history; there is no
second mutable protection table. The snapshot publishes protection with the
resource transaction, and startup reconstructs it before readiness. Existing
catalog limits bound this index. Retention must never evict those definitions to
recover capacity. The authority-format-three fence rejects predecessor writers;
profiled configuration plaintext has envelope format four and its own
authenticated compiler subtype. Legacy compiler bytes retain their old binding.
The format-four compiled document keeps each profiled projection in its original
immutable artifact. Its visible execution route is reconstructed once from that
artifact and the bound configuration defaults. The stored ordinary route sequence
must match the corresponding original artifacts exactly. Reconstruction uses the
existing cumulative byte bounds and charges the shared operation budget before
copying. Immutable resource artifacts and hidden ordinary child projections keep
their representation; the complete persisted plaintext still has a 1 MiB limit.

Effect eligibility is checked separately from grant-only reads. Historical
activation, queued admission, cached execution, provider arm, continuation,
counting and OAuth effects consult current route-key protection. The authority
guard spans the final check and arm, so publication acknowledgement prevents new
eligible arms. Already-armed work keeps its terminal obligation. Operational
state is process-local, intended for one owning instance, and cannot provide a
distributed RPM or circuit guarantee across processes or restarts.

Reached-hop admission follows the exact selected ordinal, after request guards
and before constructing or authenticating the provider request. A successful
entry charges its shared window once and supplies a private single-use arm
token. Cancellation, construction failure or a later authority refusal does not
refund that timestamp; arming rechecks authority without charging again. Exact
60-second timestamps remain counted. Windows have a 16 MiB timestamp-allocation
budget, independent of the profile's RPM ceiling; pressure refuses locally and
never evicts live history. Retained-resource limits also bound group/health keys.

Manifest observation takes one synchronous read snapshot for all candidates.
Its optional read-only group association shares the same counter namespace but
does not enable rate admission or acquire another member's circuit probe. Reads
do not allocate windows or reconstruct them from receipts. No lock crosses I/O.

Tenant pipelines reuse the configured process OpenTelemetry exporter. Trusted
execution scope, original ordinal, armed AttemptId, actual provider status and
typed terminal outcome are emitted through the existing bounded exporter.
Weak execution liveness protects an active stream from age-based span cleanup;
it does not keep execution or probe ownership alive. Exporter absence, sampling,
queue loss or export failure cannot change admission, receipts or recovery.

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

Ingress defaults to 128 uploads and 64 MiB of reserved body bytes, selected by
`tenancy.capacity.ingress`, with a
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
storage allowance. `tenancy.capacity.receipts` defaults to a 768 MiB admission
free-space floor, an 8 GiB live-receipt high-water budget and a 256 MiB maintenance
reserve. Accounting includes a conservative 16 KiB row allowance. Each grant
batch refreshes free space under the accounting mutex and includes outstanding
physical charges for management, receipt and repair writes. Physical write
amplification and filesystem exhaustion still require measurement and fault
qualification. Terminal archived history is accounted separately from live
receipt bytes; neither operation IDs nor retained history has a lifetime quota.

Unknown obligations default to admission budgets of 100 per tenant and 1,000
per instance, configurable in the same group. The transactionally maintained
indexed projection covers both receipts and pending OAuth work, deduplicated by
execution. Startup validates its complete count and digest without retaining a
vector of all unresolved identities. A cold tenant uses an indexed count under
the existing read permit and actor transaction; a pin retains that count through
admission. Live committed deltas update pinned counts, and idle tenants need no
resident count entry. Global pressure is checked before a cold count and again
at grant time. Lowering a budget does not invalidate retained evidence.
Indexed, resumable maintenance escalates unresolved work after 15 minutes
without pruning or reclassifying it. The 30-second cleanup window is not
permission to forget an uncertain transaction or an unjoined producer.

The internal authority store uses format 2 so older strict readers refuse it.
Healthy format-1 stores upgrade during complete validated startup. A legacy
count-saturated store instead requires the explicit local
`bro tenancy offline upgrade-capacity` workflow under exclusive stopped custody.
The command checks the expected installation/revision, the actual legacy count
latch, all retained authority/evidence and an explicit acknowledgment before
atomically recording the transition and its replay result. It preserves
identities and evidence and does not clear other storage or cleanup fences.
See the [operator procedure](../../skills/bitrouter/references/cli.md#local-db-only-offline-evidence).
Public strict-v1 request, attempt and evidence limits remain independent of host
capacity, as do the bounded offline bundle and declared provisioning profiles.

## Receipt archive and physical storage

The existing writer moves eligible terminal receipt and usage-evidence payloads
into an archive table in the same SQLite database. It uses bounded compression
and preserves the original bytes and integrity digests, retaining the indexed
receipt identity and correlation metadata. Readers transparently decode one
bounded payload through the same authorized receipt/page APIs. There is no
external archive service, history-row ceiling, age-based deletion or operation
ID reuse.

Maintenance begins before the live high-water budget is exhausted. Each turn
examines a bounded key page and relocates at most one eligible payload, retaining
a cursor across turns. The writer rechecks live execution custody, unresolved
obligations and pending native/paid ownership before the archive transaction.
Unknown effects remain inline. Compression runs outside the authority publication
gate; only a confirmed COMMIT moves the accounting charge from live bytes to
history. An uncertain transaction retains its real owner and reservation.

Live receipt, history and OAuth byte values are conservative logical estimates.
They are not the database file size, SQLite freelist capacity, WAL size, measured
RSS or physical bytes returned to the filesystem. Compression may leave the file
unchanged or temporarily increase writes. Admission uses a fresh volume free-space
probe plus outstanding physical write estimates; maintenance retains its own
write custody and cannot manufacture free-space credit from a predicted ratio.

With tenancy enabled, the native execution journal shares the database's physical
write budget. Journal appends use ordinary capacity; a refused append retains
recovery responsibility and cannot establish a clean stopped owner. Recording a
valid stopped-owner proof uses maintenance capacity. Native terminal appends have
no separate completion reservation guarantee. Gateway receipt completion retains
its existing protected maintenance path. Without tenancy, native storage keeps
its existing behavior.

The database lease excludes cooperating writers to that database, not other
users of the filesystem. Concurrent external consumption, filesystem metadata,
SQLite page/index layout and journal or WAL amplification can still cause ENOSPC
after a successful probe. Budgets therefore provide admission control rather
than an absolute allocation guarantee. Operators must monitor actual volume
space and qualify write amplification, archive progress, crash recovery and
mixed live/history reads for their deployment. Capacity defaults and synthetic
directory fixtures do not establish throughput, latency or a deployment maximum.

## Capacity diagnostics

Startup traces the effective capacity configuration. The existing telemetry
`receipt_latency.enabled` opt-in, which requires enabled OTel metrics and tenancy,
also adds fixed fields to `bitrouter.receipt.capacity.bytes` and
`bitrouter.receipt.capacity.state`. It uses the existing provider and reader;
disabled telemetry creates no capacity observer. Callbacks only try-lock existing
owners and copy numeric accounting. They do not scan entries, query SQL, probe
the filesystem, reclaim caches or retain the Service.

The current-only `execution_cache_*` and `credential_cache_*` fields report
resident entries and charged bytes, including pinned entries and reserved cache
metadata. `routing_*` reports target/provider entries, charged bytes and active
guards. These values describe the existing budgets, not measured RSS. Each
component has its own `*_available` field; zero omits that component's unreadable
values rather than reporting zero usage. These current working-set fields are
independent of the receipt ledger's immutable initial/final checkpoints.

`observed_free` is the last successful owner-side volume probe. It may already
be stale and does not promise a later write will fit. `volume_available=0` omits
it for an unbound volume. Logical receipt/history bytes and estimated physical
write reservations remain separate fields. Admission `closed` stops new work;
`fenced` describes the hard receipt-accounting fence. A soft receipt failure can
close admission while retained owners still reconcile against trusted accounting.
Neither flag establishes a transaction outcome.

Authority debug traces report checked bytes **as of** registration/replacement,
after releasing the publication gate. They are neither a live registry count nor
a lifetime accounting ledger. Startup phase traces and rate-limited directory
page progress identify completed validation work without row identifiers. The
final startup trace publishes four directory totals and ordinary/safety operation
totals from the validated authority snapshot only after all startup phases and
transaction cleanup succeed. Its revision identifies that as-of snapshot; it
does not claim live counts after later mutations. Partial phase progress is not a
percentage or a readiness claim. Existing admission decisions and scheduled
archive/obligation turns emit finite refusal/progress/lateness debug fields,
without per-tenant labels, backlog queries or another worker.

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

Ordered per-call canonical defaults use a closed body object and a digest in
selection identity. Ingress validates and removes the object before protocol
parsing; immutable route-cache entries never hold per-call defaults. Merge the
nine fields into canonical parameters before retained fallback defaults and
policy validation. Typed null and empty stop preserve default filling, while
opaque source extras block canonical promotion. Exact historical records must
continue through their original read codec; adding optional defaults does not
make an older binding with a missing enforcement mode valid send authority.
