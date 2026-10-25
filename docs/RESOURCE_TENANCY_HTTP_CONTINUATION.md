# Owned HTTP continuation profile

This describes the HTTP continuation consumer of the existing tenant authority
and gateway pipeline. Production dispatch, revision-bound dispatch and
strict-receipt capability flags remain false. See
[the implementation tracker](RESOURCE_TENANCY_IMPLEMENTATION.md) for this
consumer's acceptance evidence and the remaining delivery stages.

## Ownership and request binding

An ordinary Responses request uses the existing public gateway continuation ID.
Its private lookup identity binds the installation, tenant, stable principal and
full retained route reference, including the route revision. Caller-key rotation
may preserve that principal. Another principal, route, installation or tenant
cannot resolve the mapping, even when provider secrets and visible names match.

Each invocation independently binds its immutable execution configuration and
rechecks current grants. The issuing execution revision is retained as provenance;
it does not replace the next invocation's explicitly selected configuration.
Resume must still match the original candidate, credential resource reference,
actual authenticated request authority and effective reasoning effort. Equal
secret bytes alone do not authorize another credential reference.

The literal ingress route remains bound through the immutable original model
name while the existing planner prepares its internal selector. First entry
also validates the unprepared prompt's model. The final candidate and actual
request still pass the existing authority and send gates.

Raw provider response IDs and conversation fields cannot bypass the owned lookup.
Reserved continuity defaults are ineligible in retained executable profiles.
The actual rendered request is checked again, including fresh requests and
cross-protocol requests, before any privately resolved provider ID is substituted.
Integrity-only reads of existing configuration remain distinct from admission.

## Publication and output evidence

Only the authenticated executor can create the private source proof. It records
the actual attempt and target, storage evidence and terminal output. Public
request fields, finalization metadata and callback results cannot construct it.
JSON and SSE share the same proof contract; an SSE terminal must agree with the
decoder and the complete stream. A trailing error prevents publication.

Decoded delivery output and complete reconstructible provider output have
separate commitments. A causal prefix is retained only when both agree. An
image input or a lossy provider-output projection may still support exact
provider-backed resume without a causal prefix. Such a mapping cannot authorize
visible-history detachment. The private post-hook output comparison remains
mandatory even when no causal prefix exists. Explicit `store: false` creates no
mapping, regardless of a contradictory provider claim.

Preparation, delivery activation and compensation use the existing continuation
registry and required-delivery handshake. Enqueueing output is not a delivery
acknowledgment. A failed or abandoned request cannot expose a provisional
mapping. Guarded trajectory settlement remains a separate obligation;
unqualified guarded-parent correlation stays fenced.

Caller-key revocation or rotation does not cancel completion of an already
admitted provider request. Its internal owner-bound finalization may finish;
the old key cannot authorize a later fresh request or resume. A replacement key
for the same principal must independently satisfy current authority. Tenant
suspension or deletion has a separate publication boundary: the authority actor
checks current tenant lifecycle before preparation, delivery transition and
activation. Compensation remains available for the admitted owner.

A rejected request is distinct from unresolved cleanup. Only the concrete tenant
continuation finalizer can pair its exact preparation receipt with a private
actor acknowledgment for the same publication ticket and origin. The actor
produces that acknowledgment after confirmed transaction completion. This permits
a failed request with confirmed compensation to release its capacity while
preserving its error and observed usage. Generic finalizer failures, missing
acknowledgments and uncertain transactions still retain ownership. An empty
pending map or a successful public rollback callback is not such a proof.

## Storage and lifecycle

Migration034 adds three nullable ownership columns and scoped indexes to the
existing `provider_continuations` table. It does not assign historical rows to a
tenant. Legacy consumers require NULL ownership projections and the historical
cipher version. Existing column definitions, checks, payload bytes and indexes
remain intact. Downgrade refuses populated provenance or incompatible dependent
objects before changing the schema.

Owned records use a domain-restricted capability derived from the acknowledged
installation/key epoch introduced by migration031. Ownership and publication
metadata are authenticated with the encrypted payload. Startup refuses retained
owned rows with a missing or replacement epoch, incomplete provenance or an
unavailable key. No ambient Legacy key is loaded for the tenant registry.

All writes run in the existing authority actor. SQL store helpers borrow that
actor's transaction; they neither open a separate writer nor consume its commit
or rollback. Updates and deletes compare the exact retained row bytes on every
backend. A uniqueness conflict cannot adopt another owner's row.

The bounded pending map retains queued publication generations until an
acknowledged outcome. Lookup jobs retain the existing read permit through
transaction cleanup, even after their caller disappears. Cancellation only marks
abandonment; ambiguous transaction completion retains actor and file ownership.
The close-producer acknowledgment is monotonic and reusable after actor shutdown.

Maintenance uses bounded keyset scans for stale, expired and purged records.
Expiry creates an authenticated tombstone; it does not reinterpret the record as
Legacy. Production startup starts the owned reconciler after acknowledged
initialization. Low-level transaction test fixtures control that timer explicitly
so injected faults target the intended actor operation.

This profile adds no runtime crate, transport API, new permission or distributed
authority. It does not provide strict execution receipts, exactly-once inference,
crash-proof delivery or tenant ownership for the remaining harness and learning
consumers.
