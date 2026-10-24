# Resource tenancy: M0 contracts and migration inventory

Status: M0 design/fixture delivery, 2026-10-25. Proposed tenant endpoints below
are **not implemented or advertised by the gateway yet**. M1 implements local
authority; M2 implements owned configuration/state; M3 implements strict receipts.

Baseline: `38ddfd231f053beae9b52a61443a3706688628e0`, stacked on the existing
shared-concurrency change. The service/crate refactor, HTTP SDK, embedded telemetry,
optional HTTP/2 and shared gateway are prerequisites already present in that tree.
This milestone does not reimplement them or require multi-instance operation.

## Ownership and implementation boundaries

The runtime owns resource tenancy, fixed grants, configuration revisions,
learning/scheduling state, technical admission and execution evidence. An external
identity/business controller owns its directory, login and commercial rules.
Harnesses execute tools and retain their complete sessions. Runtime code stays in
`apps/bitrouter`; public value contracts ultimately live in `bitrouter-ai`, with
transport in `bitrouter-sdk`. M0 freezes wire fixtures before adding M1 public APIs.
No new runtime crate, transport, external database or identity service is required.

Base gateway owns local authority, routing resources and receipt lookup. `core`
adds learning/scheduling operations; `tui -> harness -> mcp` stays unchanged.
`http2` remains optional/default-on. Strict local receipts are an operator runtime
mode. Existing shared APIs keep their contracts; shared admission's API-key ID as
principal and user ID as tenant are legacy mappings, not the new identity model.

## Wire and authority contract

The JSON Schema and examples in `contracts/resource-tenancy/` freeze proposed
payloads. `cargo run -p dist-helper -- tenancy-contracts` checks schema shapes,
local cross-field invariants, the actual migration example, inventory paths,
operation coverage and capacity bounds. Fixture `valid` means schema validity;
optional `semantic_valid` applies only to shape-valid fixtures and defaults to
true. A shape-valid semantic negative must fail the local checker. These checks
are not an authorization server. `DispatchEligibility` is an offline fixture
pair, not an HTTP DTO. Scenario fixtures remain M1-M3 HTTP/store obligations.

### Fixed authority and resource scope

An **installation operator** is authenticated by the existing privileged host
control authority. It is never a tenant role and cannot be acquired through
BindingPut or CredentialCreate. Only installation operators create principals or
tenants, change tenant lifecycle, provision audit readers, grant operator-pool
access or detach a binding's managed writer. A **tenant_operator** has only the
fixed tenant permissions below, within its explicit scope.

| Tenant role | Permissions |
| --- | --- |
| viewer | resource.read, receipt.read |
| editor | Viewer plus resource.draft, execution.submit, credential.use, evaluation.submit |
| tenant_operator | Viewer plus policy.activate, credential.manage |
| tenant_admin | Union of tenant roles plus binding.manage, credential.issue |

Editor and tenant_operator are distinct permission sets, not an implied hierarchy.
Neither a tenant_operator role nor credential.manage alone authorizes inference.

`GrantScope` is either `{ "kind": "tenant" }` (explicitly this tenant) or
`{ "kind": "resources", "resources": [ResourceKey, ...] }`. The resource
list must be nonempty and unique; an empty list is invalid_request, never a
wildcard. ResourceKey identifies owner_kind (tenant or operator_pool), owner_id,
resource_kind and resource_id. ResourceRef adds an immutable revision_id. Route
and policy positions enforce their respective resource_kind. Tenant-wide scope
does not include another tenant or implicitly include operator pools. It may add
an explicit bounded `operator_pools: [ResourceKey, ...]` allowlist containing only
operator_pool keys; omission means no pools. This permits tenant-wide operations
using explicitly granted platform routes without an implicit pool wildcard.
Pool references require a
live installation-operator-managed binding containing the exact pool resource key
AND a matching key in the credential scope (resources or operator_pools). That binding is the v1 pool grant;
only installation operators may create/update bindings containing pool keys.
Tenants cannot delegate pool ownership or mutate an operator pool. Names and content hashes grant nothing.

A credential names exactly one recipient binding_id; its principal must match
that binding and its tenant must match the path. Effective authority is the
intersection of credential permissions/scope/expiry, that binding's CURRENT
role/scope/expiry, tenant lifecycle, and installation-operator ownership of any pool binding. Reducing or
expiring the binding reduces every attached credential immediately. Issuance and
binding delegation must fit the issuer's effective permission, resource and
expiry ceiling as well as the recipient binding; disjoint bindings cannot be
combined to manufacture broader authority. A resource-scoped writer must cover
every referenced resource in the proposed configuration or delegated grant.
Activating or rolling back a whole-tenant snapshot additionally requires
tenant-wide policy.activate in BOTH the current credential and recipient binding.
Covering just the proposed references is insufficient: removal of routes and
changes to tenant limits or learning settings affect resources outside a narrow
scope. A resource-scoped writer may prepare an authorized draft but cannot activate
it, even if it is empty or the writer knows the current CAS revision. Explicit
operator-pool checks remain necessary for tenant-wide activation.
Credential management covers only same-tenant credentials whose grants fit the
manager's scope. Receipt reads and active-snapshot reads require tenant-wide
scope; a resource-scoped role alone cannot enumerate tenant facts. Installation
operators are exempt; the dedicated audit reader is exempt only for receipt reads.
Revoking an issuer credential does not silently cascade to independently issued
credentials: their recipient binding stays authoritative. Incident containment
uses explicit revokes or installation-operator tenant suspension.

### Proposed protected HTTP operations

All new routes use the protected control listener. No endpoint is live in M0.
The tenancy/operations namespace is separate from existing host reload operations
at /control/v1/operations/{request_id}; their API and recovery behavior stay intact.
Every mutation body carries installation_id. Every GET below except capability
discovery requires `Bitrouter-Installation-Id`; it must match the serving
installation. This prevents old commands being retargeted after offline recovery.

| Method/path under `/control/v1` | Request / response definition | Authority and effects |
| --- | --- | --- |
| GET `/tenancy/capabilities` | none / TenantCapabilities | Authenticated discovery; explicit version and instance binding |
| PUT `/tenants/{tenant_id}` | TenantPut / OperationResult | Installation operator; creation or CAS lifecycle change; no client tombstone |
| PUT `/principals/{principal_id}` | PrincipalPut / OperationResult | Installation operator; immutable issuer/subject identity |
| PUT `/tenants/{tenant_id}/bindings/{binding_id}` | BindingPut / OperationResult | binding.manage; subset/resource/expiry ceiling; one managed writer |
| POST `/tenants/{tenant_id}/credentials` | CredentialCreate / CredentialIssued | credential.issue; returned credential_revision supports CAS revoke |
| GET `/tenants/{tenant_id}/credentials/{credential_id}` | none / CredentialStatus | credential.manage or installation operator; current revision/state, never secret |
| POST `/tenants/{tenant_id}/audit-readers` | AuditReaderCreate / CredentialIssued | Installation operator; fixed tenant receipt.audit.read only |
| DELETE `/tenants/{tenant_id}/bindings/{binding_id}/management` | Mutation / OperationResult | Installation operator detach; controller cannot regain ownership implicitly |
| DELETE `/tenants/{tenant_id}/credentials/{credential_id}` | CredentialRevoke / OperationResult | credential.manage or installation operator; non-null observed credential revision |
| POST `/tenants/{tenant_id}/configurations` | ConfigurationPut / ConfigurationRevision | resource.draft; typed bounded document, immutable content revision |
| POST `/tenants/{tenant_id}/activation` | Activation / OperationResult | Tenant-wide policy.activate; CAS active revision, current reference/policy authority |
| GET `/tenants/{tenant_id}/activation` | none / ActiveRevision | Tenant-wide resource.read; durable active snapshot |
| GET `/tenants/{tenant_id}/configurations/{revision_id}` | none / ConfigurationRevision | resource.read covering document resources |
| GET `/tenancy/operations/{operation_id}` | none / OperationResult | Original principal AND current authority for the saved tenant/resources or installation class |
| GET `/tenants/{tenant_id}/executions/{execution_id}` | none / ExecutionReceipt | Tenant-wide receipt.read or dedicated audit reader |
| GET `/tenants/{tenant_id}/executions?correlation_id=...&after=...&limit=...` | none / ExecutionPage | Same read authority; 200 paginated results, limit 1..100 |

Installation operators can perform tenant management without manufacturing a
tenant binding. The special audit reader bypasses inactive-tenant read denial
only for receipt reads; it has no execution, resource mutation or issuance grant.
Tenant lifecycle creation uses expected_revision:null; updates require the
observed revision. Binding creation uses null for both expected_revision and
expected_management_owner and claims the authenticated writer;
updates compare both revision and expected_management_owner. Installation-operator
detach clears management ownership; ordinary controllers cannot claim an existing
detached binding. Principal issuer/subject cannot change on a CAS update.

### Eligibility bound to dispatch

TenantCapabilities v1 freezes contract_version:1, installation_id (stable store
identity), instance_id (fresh every process start), capability_revision (fresh on
any eligibility/mode change), revision_bound_dispatch, strict_receipts and
dispatch_enabled. Eligibility requires all three booleans true. Missing discovery,
unsupported version or a disabled flag keeps new traffic on an otherwise eligible
existing backend before submission. The current baseline advertises none of this.

DispatchBinding specifies the exact trusted HTTP metadata:
`Bitrouter-Tenancy-Version: 1`, `Bitrouter-Installation-Id`,
`Bitrouter-Instance-Id`, `Bitrouter-Capability-Revision`,
`Bitrouter-Execution-Revision`, `Bitrouter-Correlation-Id` and
`Bitrouter-Receipt-Mode: strict`. The gateway must atomically compare the current
version/installation/instance/capability binding and strict eligibility at admission,
and recheck eligibility before every new provider attempt/wake. A stale descriptor
cannot downgrade a strict submission. Capability mismatch rejects before provider
work; errors during already-started work retain started/unknown dispatch evidence.
A refresh permits a new explicit caller decision, never automatic inference replay.
No token signs an entitlement into existence: transport authentication, tenant
binding and current resource grants still must agree. Public input cannot override
tenant or principal. The trusted adapter strips external copies before setting
submission metadata; duplicate/ambiguous metadata is invalid_request. Existing
inference bodies and response framing stay protocol-compatible.

### Command identity, secret replay and revocation

Operation identity is exactly (installation_id, authenticated principal_id,
operation_id), independent of tenant and operation kind. The lookup above derives
the principal from authentication; delegation of operation lookup is unsupported
in v1. The same ID used for another tenant, method, target or kind conflicts. Store
the result and command digest atomically. Digest v1 is SHA-256 over a UTF-8 JSON
object containing hash_version:1, installation_id, principal_id, method,
operation_kind, target_ids (the matched route's named IDs) and typed request body.
Object keys are recursively sorted; array order is preserved. Number spelling is
normalized by the pinned serde_json serializer, not by raw request lexemes; RFC
8785 is not claimed. Reject duplicate JSON object keys before typed parsing.
Hash version is stored; no secret is part of a command digest or diagnostic log.

Save the command's authorization class (installation or tenant), tenant identity,
affected resource scope and required result-read permission alongside its digest
and result. This is internal authorization context, never caller-supplied lookup
authority. Principal equality is necessary but insufficient for lookup or replay:
before returning a result, conflict detail or expired-operation metadata, check
the current credential, binding and lifecycle against that saved context. Tenant
results require the same tenant and coverage of every saved affected resource;
installation commands require current installation-operator authority.
Configuration results require resource.read (tenant-wide for activation);
credential results require credential.manage covering the affected grants; binding
results require binding.manage covering the affected binding. Issuers needing
lost-response recovery must also be provisioned the corresponding read authority.
Failure returns a generic forbidden/not-found result without foreign IDs or
revisions. A credential for tenant-b cannot read tenant-a results even for the
same principal. Narrowing scope removes historical result access. Credential
rotation can still recover results when the stable principal and current tenant,
scope and read authority remain valid. A replay never reapplies a committed
mutation to recover lost read access; tenant suspension does not implicitly grant
operation-read access to a receipt-only audit reader.

An applied OperationResult contains non-null resource_id/resource_revision and
authority_revision. Credential issuance results refer to credential ID/revision;
configuration results refer to the stored configuration ID/revision. Lookup stores
only this nonsecret result. The initial successful issuance response alone may
contain secret. Concurrent/late replays return the identical credential identity,
credential_revision and secret:null. The loser of concurrent issuance cannot
receive the secret. After a lost response, use replay or operation lookup to obtain
the ID/revision, read CredentialStatus if the revision is stale, and submit a new
CAS revoke or rotation. Never recover a secret from operation storage.

Revoke requires a non-null credential revision and publishes the new authority
revision before applied acknowledgement. A new operation against an already revoked
credential still compares its revision and returns its current revoked result;
only a matching operation replay bypasses reapplication. Rotation must target an
active credential of the same tenant/principal/binding and cannot broaden its
grants. It atomically issues the replacement and revokes the predecessor with one
authority publication; no overlap window is implied. Audit-reader rotation in v1
uses installation-operator issue then revoke, with explicit overlap. In-flight
provider work may finish under its owner-bound finalization capability; a revoked
credential cannot authorize another attempt, wake or promotion.

### Operation capacity and offline recovery

Results are retained for 30 days; afterward the ID remains a tombstone returning
operation_expired, never reapplying the command. The instance ceiling is 100,000
retained IDs INCLUDING 512 reserved safety IDs. Ordinary commands cannot use the
reserve. At 99,488 ordinary IDs, atomically persist a saturation latch and stop all
new inference/learning admission, new attempts/wakes and ordinary mutations,
including legacy/non-strict ingress on that installation. Advertise
dispatch_enabled:false and change capability_revision. Do not resume merely because
results aged into tombstones or a process restarted.

Only installation operators may consume the safety reserve, solely for credential
revoke and transitions to suspended/deleting. It covers all 256 live credentials
(including audit readers) plus two stop transitions for each of 32 tenants: 320
IDs, with 192 spare IDs for conflicts/recovery. A safety command ID is counted even
if rejected after admission. No creation, rotation, activation, detach or reactivation
uses that reserve. Exhaustion or unavailable authority storage leaves dispatch
fenced; it never evicts tombstones. Receipt reads, owner-bound finalization and
existing host stop controls remain available where their storage permits.

Offline recovery is an installation-operator procedure, not a new public API:

1. Stop listeners/admission and fence every state-store and selected credential-file
   writer. Drain/classify outstanding work; retain unknowns without re-execution.
2. Preserve a read-only backup with installation identity, all command tombstones,
   receipts, owner mappings, pending refresh classifications and counts/digests.
   Never delete tombstones or reopen that history for mutations with an old binary.
3. If more writable capacity is needed, provision a separate state directory with a
   fresh installation_id and fresh ingress credentials. Re-provision only explicitly
   evidenced configuration/ownership; no operation history or uncertain invocation
   is replayed. Old commands carry the retired installation ID and are rejected;
   old credentials are not accepted. Retained old receipts remain under the old
   installation's read-only audit service and original execution IDs.
4. Reconcile pending/unknown work and selected upstream credential slots explicitly,
   verify fencing/counts/digests, then authorize new admission on the fresh instance.
   Do not merge writer histories or automatically retry an uncertain invocation.

IDs are bounded nonempty ASCII, never recycled or derived from content hashes.
Stable principal identity survives credential rotation and process restart.

### Errors

Errors use the Error schema and contain no secrets or foreign resource details.

| HTTP | Code | Meaning / retry rule |
| --- | --- | --- |
| 400 | invalid_request | Invalid shape, unsupported host configuration or ambiguous metadata; no dispatch |
| 401 | unauthorized | Missing/invalid authentication; no dispatch |
| 403 | forbidden / tenant_inactive | Scope or lifecycle denies action; no dispatch |
| 404 | not_found | Owned resource absent or hidden; no existence disclosure |
| 409 | revision_conflict / operation_conflict / capability_mismatch | No implicit current revision, target or instance substitution |
| 410 | operation_expired | Retained command tombstone; never execute that ID again |
| 413 | payload_too_large | Reject before compile/provider work |
| 429 | capacity_exceeded | Explicit rejection; bounded Retry-After only for transient capacity, absent for the persistent saturation latch |
| 503 | authority_unavailable / storage_unavailable | Fail closed; execution outcome can be unknown, no automatic inference retry |

Correlation reuse is not an error and never means idempotency. Its query returns
200 ExecutionPage, including when more than one execution matches.
`dispatch_state` is not_started, started or unknown; only positive evidence allows
not_started. A network timeout has no trustworthy response body and is unknown.
Management reads may retry. Existing protocol error envelopes remain compatible;
the SDK maps authenticated metadata without inventing a successful terminal.

## Version binding, lifecycle and receipts

Runs bind an immutable execution revision including all routing/policy/checkpoint
references capable of changing the target set. New active R2 cannot replace an
approved R1 between intent and dispatch. Current permission/revocation checks
still apply at each attempt/wake/promotion. A target envelope may narrow due to
health or resource limits; it cannot expand. Missing retained R1 fails before work.
Drafts may name only owned resources or explicitly granted operator pools. No
upstream URL, environment expansion, file path or executable upload is accepted.

Suspended/deleting tenants cannot dispatch, wake, publish or promote. Previously
admitted work can finalize under an internal owner-bound capability. A separately
provisioned `receipt.audit.read` service grant can read retained facts during
suspension/deletion; it cannot mutate, execute or restore user access. Tombstoning
requires drain/fencing and classification of unknown work, and preserves audit
reads until retention deletion. Lifecycle recovery is operator-only.

Every actual invocation has a fresh installation/execution identity. Correlation
is not idempotency. Receipts capture actual provider/native model, nonsecret
account/credential version, configuration revision and bounded per-attempt usage.
Unobserved quantities are null with incomplete/unknown provenance, never zero.
The native_usage whitelist is input_tokens, output_tokens, cache_read_tokens,
cache_write_tokens and reasoning_tokens, each at most once per attempt. Unsupported
native dimensions mark evidence incomplete and cannot justify synthetic charges.
Unknown provenance requires null quantities and complete:false; complete usage
requires every listed quantity to be observed. Attempt IDs are unique per receipt;
a completed attempt requires its actual target, and a completed invocation has at
least one completed attempt. Receipt totals cannot substitute a known subtotal
for an unknown total. Byte limits apply to serialized evidence as well as counts.
Aggregate provenance is conservative across EVERY attempt: unknown if there are
no attempts or any attempt has unknown usage; otherwise estimated if any attempt
is estimated; otherwise provider. Unknown aggregate quantities remain null.
Estimated usage is always complete:false. Aggregate complete:true additionally
requires every attempt's usage to be complete and all required quantities known;
numeric equality alone cannot establish completeness. A receipt may remain
incomplete when additional evidence is missing. Keep each attempt's original
provenance; summing numbers never upgrades an estimate to provider evidence.
Strict mode commits terminal facts before reporting durable completion and
propagates storage failure. Ordinary recorder behavior is not strict durability.
No pre-dispatch log, complete crash recovery or exactly-once inference is claimed.

ExecutionPage includes installation_id and correlation_id. Execution IDs are
unique and strictly ascending by ASCII byte order; after is exclusive. Non-null
next_after equals the last returned ID and means more matches were observed.
multiple_executions is query-wide across retained readable matches, independent of
page size; a one-row last page may still report true. Empty results have null
next_after. A newly observed execution may require restarting from no after;
clients deduplicate installation/execution IDs. There are no delivery cursors or
ACKs. Retention cannot hide admitted completions. External consumers keep submission intent and must not treat missing evidence as unexecuted.

## Owner/query/cache inventory and migration

`contracts/resource-tenancy/inventory.json` classifies current source families.
Each entry records existing authority, migration owner, affected readers/writers,
cache/background obligations, and explicit exclusions. M0 does not execute a data
migration or invent live row counts. The deployment inventory must attach actual
counts and hashes to the manifest before M1/M2 cutover.

| Current family | Existing key / hazard | Required disposition |
| --- | --- | --- |
| auth / policy | user ID, API-key row, policy IDs | Stable tenant/principal/credential mapping; preserve historical IDs, do not alias key rotation to a new person |
| shared authority | API-key ID => principal, user ID => tenant | Compatibility adapter with explicit old/new mapping; local mode never runs through shared transactions |
| eval subjects/results/snapshots | owner_user_id and content hashes, operator unscoped paths | Tenant predicates on every reader/writer; principal provenance; no private cross-tenant aggregation |
| evolution | hash(owner), block/experiment/checkpoint keys | Tenant-scoped store keys plus manifest mapping; rekey materialized caches and background work |
| sessions/workflow/scheduling | owner/source/native session, run/checkpoint identities | Owned scheduler records and parent/child references; harness session authority unchanged |
| trajectory/capture | artifact/evidence roots, retained raw observations | Owner checks before content lookup; scoped cleanup, privacy/retention limits |
| continuation | installation + owner digest + response/credential bindings | Preserve original backend and subject; no migration by hash/string substitution |
| metering/execution | user/key/request/attempt identity, best-effort recorder | Scoped durable receipt adapter; ordinary accounting stays compatible |
| host config/control | global snapshots, reload and operation IDs | Preserve privileged host boundary; add separately scoped tenant commands |
| credential/provider/router caches | global provider/account or model keys | Explicit operator pools or tenant-owned slots; authorization revision in private cache keys |

Manifest key: (source_installation_id, source_kind, old_owner, resource_kind).
source_kind is an owner namespace: local_user, shared_user and native_owner with
the same text are distinct owners. Every key appears exactly once, even for
identical duplicate mappings. All mapping installation IDs must equal the wrapper.
A mapped row requires a proven old owner and tenant/principal. source_kind:unresolved
requires old_owner:null and quarantined disposition; quarantine never grants access.
All unresolved rows of a resource family are aggregated into its one quarantine
entry, with evidence identifying the affected source set.

Each mapping records source_count, source_content_digest, evidence_ref,
evidence_digest and a nonempty rationale; the wrapper pins source_commit and
source_snapshot_digest. resource_examples are bounded samples, not a row-count
proof; sample IDs must be unique within a mapping and cannot outnumber source rows.
Legacy IDs are preserved as bounded source strings, not forced into the new Id
alphabet. Same names/hashes across tenants remain separate. Mapping multiple
(source_kind, old_owner) identities into one tenant requires exactly one matching
tenant_merge_approvals record naming an installation operator, evidence digest
and rationale. A digest's syntactic validity does not prove the evidence: the
operator must verify the external evidence and actual counts/hashes before cutover.
The executable M0 checker rejects contradictory mappings and missing merge evidence.

The credential inventory includes snapshot.rs, file/backend.rs and store.rs under
apps/bitrouter/src/gateway/providers/auth. Their current keys are selected canonical
file path plus provider/account; default oauth-tokens.json can live outside the
state directory. ACCOUNTS leases and pending refresh replacements are process-local.
Inventory every selected path and writer, preserve explicit operator-pool ownership
unless a proven mapping creates a distinct tenant slot, and drain or quarantine
pending replacements. Fencing only the state-directory owner is insufficient.

Cutover: additive columns/tables -> backfill -> compare counts/hashes -> stop new
admission -> drain -> fence old writer -> activate owned reads/writes -> restart
verification. Queued work must carry tenant before wake. One local owner per
state directory; old binaries cannot open a tenant-aware live store. Rollback
uses a compatible binary or an isolated backup plus explicit reconciliation.
Do not merge two writers' histories or fail over an uncertain invocation.

## Frozen capacity and benchmark profile

`benchmark-profile.json` is the M0 reference input, not a measurement report.
Reference host: Apple M4 (10 CPU cores, verified), macOS arm64, 24 GiB RAM,
local APFS/SQLite WAL, one owning process.
Pin compiler/build/artifact hash and free-space facts with each actual run.
Development measurements cannot establish production release performance.

Configured v1 limits: 32 tenants, 4 principals and 2 live credentials per principal
(256 live credentials, including audit readers; rotation replaces one slot);
16 active executions/tenant, 128/instance, 32 queued/tenant,
256/instance; 2 learning jobs/tenant, 8/instance. Queue wait 5 s; request 60 s;
stream/cleanup drain 30 s; management/validation 5 s. Maximum 8 provider attempts,
256 total route-plus-policy references (255 routes when policy is present) and
1 MiB configuration; 1 MiB terminal receipt including all attempts, at most 8 KiB numeric native-usage evidence/attempt. Exceeding an
observation bound yields incomplete bounded evidence, never truncates silently.

Each admitted strict execution reserves 4 MiB of storage budget: 1 MiB bounded
receipt plus 3 MiB initial WAL/index/transaction allowance. At 128 in-flight,
reserve 512 MiB, plus 256 MiB maintenance floor. Stop strict admission when free
space is below 768 MiB or the 8 GiB receipt high-water mark is reached, whichever
comes first. Reservations are atomic with admission and released only after
terminal disposition. The 4x allowance is a design assumption to validate against
real storage amplification; violation blocks rollout and requires re-sizing.
A durable unknown terminal disposition may release its completion reservation;
work without durable disposition retains its reservation and in-flight accounting.
Unknowns escalate after 15 minutes into bounded queues (100 per tenant, 1,000
per instance). A full unknown queue stops new admission instead of evicting facts.
Investigation retention is at least 30 days, with no automatic classification as
success or non-execution. Commercial exposure limits belong to
the external controller. No disk-space reservation claims protection from unrelated external writers.
Keep a dedicated bounded state volume and test ENOSPC; completion can still be
unknown if the filesystem fails. Receipts do not auto-prune in the first release.

Evidence: warmup 15 s, measurement 60 s, drain 30 s, five paired/interleaved runs
for both H1 and explicit h2c. Exercise concurrency 1/16/64/128, instantaneous JSON,
32 chunks at 10 ms, two client function tools, and 100 ms slow reading. Use a fresh
mock per run; hold TLS, auth, storage, features and workload constant. Record TTFT,
p99 end-to-end, successful throughput, failures, idle/steady RSS, receipt latency
and upstream invocation counts. Missing measurements stay unknown. Include an
unmodified direct-provider calibration to detect runner saturation.

The existing standalone `apps/bitrouter/tests/fixtures/gateway_measurements`
runner remains the deterministic local H1 process/RSS baseline; it does not
measure TTFT or h2c. The complementary reusable external TS/Node load runner is
pinned in the integration evidence. M0 validates their invocation and fixtures;
M5 performs controlled paired production qualification, after strict receipts and
tenant APIs exist. A profile is not proof of isolation, durability or capacity.

## M0 acceptance and follow-up

- Source inventory paths resolve and migration examples contain two owners with
  overlapping names/hashes, separate owner namespaces, evidenced merges and
  quarantined ownership; the actual migration-example.json is validated.
- Wire fixtures cover lifecycle, delegation bounds, CAS, replay conflicts,
  version/instance-bound eligibility and multi-page correlation; shapes reject
  host URLs, empty resource scopes, aggregate reference overflow and malformed IDs.
  Local semantic negatives cover conflicting mappings, missing merge evidence,
  stale dispatch bindings, repeated attempt identities and inconsistent capacity.
- Dependency spike uses the existing HTTP SDK in a separate consumer and keeps
  obsolete server assembly out of public contracts. Compatibility is measured,
  not inferred from crate names.
- Capacity/profile inputs and reproducible runner entry points are recorded.
- Independent review must close contract gaps before M1 implementation.

M0 validation on this candidate: 129 wire fixtures (including shape and semantic
rejections), the actual migration manifest and capacity profile pass the helper.
The final dist-helper suite passes all 65 tests; strict helper Clippy and formatting
pass. The full workspace run passed 4,972 tests with 160 opt-in tests skipped;
the later contract-only revisions were verified with the complete helper suite.
Workspace all-feature/all-target Clippy and 12 doctests also pass (one ignored
doctest). Independent review and focused re-review closed the authority, operation
lookup, evidence aggregation, migration and capacity findings. These results do
not certify the 35 deferred live-runtime scenarios or production performance.

M1-M3 must add live HTTP/store tests for every scenario fixture. No M0 shape test
can certify authorization, tenant migration, financial behavior or performance.
