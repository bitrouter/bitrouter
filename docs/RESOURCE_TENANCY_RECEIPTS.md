# Owned execution receipts

This describes the internal strict-receipt implementation under the resource
tenancy contract. Dispatch and strict receipts follow the supported opt-in,
single-instance Unix SQLite [Service eligibility](RESOURCE_TENANCY_IMPLEMENTATION.md#current-local-dispatch-eligibility).
Source-pinned local checks do not establish complete M3, final aggregate,
platform or performance acceptance; see [remaining qualification](RESOURCE_TENANCY_IMPLEMENTATION.md#remaining-acceptance).

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
reserved before authentication and armed only after the last current-authority
check binds the actual authenticated request. Attempt facts preserve the configured
provider identifier, exact retained target and configured credential slot. They exclude credential secrets, raw account
labels, prompts and model output.

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

Migration 035 adds an owner-qualified receipt table and correlation index without
assigning legacy records. It checks the supported SQLite, PostgreSQL and MySQL
catalog shapes, preserves immutable conflicts, and refuses a populated downgrade.
The public contract crate and SDK operation set are unchanged.

## Filesystem fault qualification

An isolated macOS campaign passed on a fully allocated, capped 1.5 GiB APFS
image. A bounded filler observed actual `ENOSPC` from `sync_all`; the original
receipt transaction then returned SQLite `SQLITE_FULL` from its consuming COMMIT.
The test verified that the uncertain owner retained execution capacity and the
file lease, did not publish terminal success, and did not replay the provider.
After the child process exited, the database passed integrity checks, the earlier
immutable receipt was unchanged, and a fresh process could acquire the lease.
The exact test attachment and its owned images were removed after qualification.

This qualifies the observed APFS/SQLite failure path. It does not establish Linux
filesystem behavior, power-loss or fsync durability, distributed coordination,
write amplification, or production throughput. Those remain separate gates.
