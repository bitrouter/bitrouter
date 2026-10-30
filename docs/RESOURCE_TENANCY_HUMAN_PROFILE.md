# Owned human assessment in the local embedding

This source profile extends the original authenticated `ControlledSession` with
three in-process methods: `review_checkpoint`, `submit_assessment`, and
`retract_assessment`. It adds no HTTP, daemon or ACP request, authority constructor,
table, permission, or capability activation. Dispatch and strict receipts follow
the supported opt-in, single-instance Unix SQLite
[Service eligibility](RESOURCE_TENANCY_IMPLEMENTATION.md#current-local-dispatch-eligibility).
Source-pinned local evidence does not establish all-core, final aggregate,
platform or performance acceptance; see [remaining qualification](RESOURCE_TENANCY_IMPLEMENTATION.md#remaining-acceptance).

Review reads one existing immutable checkpoint of a successfully created native
session on that same controller. None resolves the already committed cursor;
Some names an exact historical checkpoint. The response includes cited evidence,
the fixed rubric, complete bounded history, nullable expected head, supported
prefill and the current session watermark. It does not invoke an evaluator or
freeze current traffic. Off does no discovery. Evidence can contain user content;
this is an authorized read, not universal secret redaction.

Submit uses the existing RubricSubmission. Human source is mandatory. Bounded
evaluator labels are normalized to `owned-human/<stable principal>` and
`owned-manual-rubric-v1` before hashing. The server verifies the rubric and its
citations. Retract uses RevisionInput with assessment=None and an explicit reason;
it requires the selected non-retracted assessment at that checkpoint and writes
an immutable Human tombstone. Original credential/current grants, lifecycle,
selected association and expected head remain checked. Key rotation does not
silently rebind an existing controller.

Exact normalized replay returns the original revision and timestamp before head
CAS. A changed input under the same ID conflicts. A stale expected head conflicts
without an orphan row. An older-checkpoint correction is retained unselected;
it cannot replace a newer selected watermark. A human correction/tombstone at
the held judge's checkpoint wins selection without canceling or refunding its
accepted paid work. Known model usage and receipts remain retained.

All three methods share the capture Port, finite actor and H transaction owner.
Human admission tries the Port lock without an unbounded waiter queue. It does
not occupy or widen the ordinary ACP command slot. A canceled human waiter leaves
the exact request, original five-second deadline and actor receiver in the same
Pending slot. Before another ordinary ACP effect, the owner reconciles it. A
proven not-enqueued request or confirmed ordinary domain rollback can be retired;
pending SQL, lost outcome and unconfirmed cleanup retain custody. Generic capture
failure semantics are unchanged. Sensitive result delivery is checked again.

History admission is shared with paid Accept:128 rows/8MiB per checkpoint,
1MiB per revision,16MiB complete review and one1024-row/32MiB operation budget.
An accepted paid terminal reserves one row/1MiB before dispatch. Manual writes
count persisted history plus every exact pending reservation. Finish atomically
consumes that reservation or releases only its history allowance when it writes
no assessment. It never refunds the revision-cumulative accepted-job counter.
Exact replay adds nothing. No truncation or runtime RSS claim is implied.

The private paid job/pending encoding is version2 with an exact checkpoint,
one-row/1MiB allowance and a strict held/consumed/released state. Version1 terminal
records retain their original encoding; unreserved version1 accepted records are
not adopted. Unknown versions reject. Paid stopped-owner recovery is separate and
must carry these reservation fields in its own reviewed version; this profile
does not grant recovery authority from a pending row or timestamp.

The external library example `owned_capture_handler` shows explicit review/edit
and review/retract callbacks plus polling a borrowed checkpoint alongside a manual
action. Both outcomes are retained. The embedding still owns update/permission
handling, its listener/Service, and explicit controller shutdown.

Separate obligations include authenticated editing after restart/reauthentication,
cross-principal delegation, stopped paid-job recovery, experiments and promotion.
The accepted-job accounting period is cumulative accepted logical jobs per
immutable configuration revision, no reset
on restart/reactivation, and no refund after durable acceptance. It is not a tenant
spending limit. Explicit human work consumes no paid logical-job unit.
