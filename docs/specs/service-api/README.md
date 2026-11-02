# BitRouter service API

Status: **beta, specification revision 1.0.0-beta.3**. This is the single
compatibility contract for BitRouter-owned service messages and API formats.
The revision is distinct from each existing URL namespace, ACP version, schema
version and beta-negotiation value. Beta changes can be incompatible, but must
be identified and reviewed explicitly. MUST, MUST NOT, SHOULD and MAY express
requirements on conforming peers.

## Scope and compatibility review

This specification covers request/response shapes, identity, authentication
selection, negotiation, errors, pagination and safe replay. Third-party inference
and MCP formats retain their upstream specifications; implementation, routing,
module ownership and deployment procedures live in the development and operator
guides. The sections below are parts of this one API contract, not independently
versioned feature specifications.

Changes must review old-client/new-server and new-client/old-server behavior.
Do not silently change a field's type, units, default, nullability, enum meaning,
identifier scope, error semantics, terminal event or fingerprint encoding.
Capability discovery is required where offered; an unknown optional response
field can be ignored only where that message's schema permits extensions.
Unsupported required behavior must fail visibly.
Request strictness varies by schema; adding a request field is not automatically
compatible with a reader that rejects unknown fields. Schema/DTO changes,
examples and compatibility cases must be reviewed together. Feature availability
and authorization remain distinct from format compatibility.

Revision beta.2 consolidates resource provisioning and execution usage evidence
for independently updated management clients, SDKs and receipt consumers. Their
wire discriminators remain 1; the existing strict tenancy capability and receipt
schemas are unchanged. Retained resource authority and evidence require the
migration and rollback rules below, even when an older client uses only receipts.

Revision beta.3 adds operator directory readback and explicitly negotiated
ordered dispatch v2. Strict v1 discovery, receipts and usage-evidence wire
formats retain their existing meanings and eight-attempt bound. The ordered receipt migration follows capacity migrations 042–044. The
capacity-only binary rejects its SQLite migration ledger entry, and its
immutable audit readers require the earlier exact catalog.

### Offline recovery evidence v2

Offline plan writers, provisioning recorders and independent verifiers use the
explicit v2 formats in the [offline envelope schema](offline-v2.schema.json).
These are private operator artifacts, not new HTTP endpoints or runtime
authorization. V1 retains its exact arrays, limits, fingerprints and report
shape. A v1 reader MUST reject a v2 discriminator; a v2 reader MUST select the
declared version and MUST NOT reinterpret a truncated v1 artifact as v2. This
opt-in beta profile does not change HTTP v2 or the native startup fields.

`bitrouter.offline.recovery-plan.v2` retains v1 `source` and `target` fields.
`retirement` becomes a `{path,sha256}` anchor instead of embedded writer and
selected-file arrays. The provisioning, static approval, retirement and producer
anchors MUST be distinct. SHA256 binds every byte, including LF terminators.
The source producer attestation retains its v1 shape. The schema-sized source
table dispositions remain in the root; their row/value totals are diagnostics,
not directory or history ceilings.

Each stream is UTF-8 JSON Lines: one strict object per line, a header first,
mandatory final LF, no blank lines, duplicate fields, unknown envelope fields,
extra objects or trailing bytes. Embedded objects retain their existing DTO
validation and compiler semantics. Counts MUST exactly match complete EOF;
declaring a smaller count never authorizes partial verification. Each decoded
record must fit the operator's remaining working allowance. Insufficient working
memory, scratch or free disk means refusal without a successful report; it does
not mean retained data is corrupt. Total stream cardinality has no product cap.
Readers MUST preserve exact integer counts, check arithmetic and offsets for
overflow, and observe the existing individual identity/path/artifact limits.

| Stream | Header format | Ordered records |
| --- | --- | --- |
| Observed provisioning | `bitrouter.offline.fresh-provisioning.v2` | Operations, configurations, active pointers, resources |
| Independent static approval | `bitrouter.offline.static-approval.v2` | Approved resource definitions |
| Operational retirement | `bitrouter.offline.retirement-attestation.v2` | Retired writers, then selected files |

Provisioning operation records have `kind: "operation"`, `principal_id` and the
existing `OperationResult`. Every operation MUST be applied, with exact authority
revisions 1 through `operation_count` and no duplicate operation ID. Configuration
records contain `kind: "configuration"` and `revision: ConfigurationRevision`,
ordered by `(tenant_id,revision_id)`. Active records contain `kind: "active"`,
`tenant_id`, `revision_id` and `authority_revision`, ordered by tenant. Resource
records contain `kind: "resource"` and `reference: ResourceRef`.

Both resource sections sort strictly by the SQL BINARY tuple
`(owner_kind,owner_id,resource_kind,resource_id,revision_id)`, using the serialized
enum strings rather than enum ordinals. IDs retain their ASCII 128-byte contract.
Writer labels retain their 256 **UTF-8 byte** contract and reject control
characters, including NUL. Writer order is exact UTF-8 byte order; selected-file
order is canonical absolute path order. Every file names a declared retired
writer. All fence assertions and writer `retired` flags MUST be true; they bind
operator statements, not independently established exclusion of external writers.

Approval records additionally contain `definition`, using the existing
[privileged resource import definition](../../../apps/bitrouter/src/tenancy/resources/manifest.rs).
Fresh recovery accepts static routes without exploration, paid assessment,
CoreChoice or Block; policies; and API-key credentials with no OAuth profile and
generation zero. Approval is independent of observed provisioning and MUST cover
exactly the authenticated static closure. Exact restore is a different proof:
it preserves retained history, original installation identity, resource envelopes
and OAuth generations, authenticates them with the explicit original key, and
does not apply fresh-only restrictions, import, migrate or resume the database.
Normal feature-specific startup remains a separate check.

The separate Eval authority proof uses the same provisioning stream with zero
configuration, active and resource counts. It does not require static approval
or a resource key merely to prove authority/operations; its destination mapping
and no-work-history rules still apply. The
[Eval provisioning example](examples/offline-v2-eval-provisioning.jsonl),
[approval record example](examples/offline-v2-static-approval.jsonl) and
[retirement example](examples/offline-v2-retirement.jsonl) illustrate separate
synthetic artifacts. The [root example](examples/offline-v2-recovery-plan.json)
uses placeholder digests and one illustrative table disposition. None is a
complete recovery plan or authorization; a real root requires the complete
source inventory and actual byte anchors.

The corresponding root is `bitrouter.offline.eval-backfill.v2`, specified by
`EvalBackfillPlan` in the same envelope schema and this
[synthetic root example](examples/offline-v2-eval-backfill-plan.json). It retains
v1's source `bundle`, `database_evidence_digest`, `source_kind`,
`ownership_evidence` and its digest, `manifest` and its digest, plus `target`,
`target_bundle`, `target_evidence_digest` and `target_installation`. The one shape
change is replacing `provisioning_operations: OperationId[]` with the required
`provisioning: {path,sha256}` anchor for complete operation readback. Supplying
the old field, both fields, an unknown field or an unsupported discriminator
MUST fail. Changing only the discriminator on a v1 plan is not conversion.
The provisioning header MUST match `target_installation`, contain nonzero
operations and declare zero configuration/active/resource counts. V2 does not
add `static_manifest` or `wrapping_key_env` to this Eval root. Existing source
kind, explicit owner mapping, graph/individual-object validation, transactional
transfer and provenance checks remain required; streamed database inventories
and authority directories do not silently widen a separately bounded Eval
transfer profile. Neither source ownership evidence nor successful transfer
proves that external writers have been independently stopped.

Compatibility cases MUST include v1 byte/limit behavior unchanged, explicit
unknown-version refusal, truncation/trailing-record/count mismatch, duplicate
IDs, resource string-order disagreement, NUL and multibyte writer labels,
pre-decode working-budget refusal, historical restore under the original key,
and refusal before mutating any overlapping input/output/scratch/lock path.
Successful evidence publication requires confirmed SQL close, input/copy rechecks,
scratch cleanup and a fresh actual-volume probe. Shared-storage races can still
cause ENOSPC; a probe is not an absolute disk guarantee. Evidence reports do not
release ingress, clear unknown upstream effects or establish cutover readiness.

## Control API

### Scope and discovery

The optional authenticated control listener serves operator inspection and
reload. It is distinct from the inference gateway and its credentials. A client
MUST authenticate before using `GET /control/v1/capabilities`; it MUST use the
reported resources, actions, scopes and schemas to determine what is available
for its credential and host. A missing action is not an authorization grant.
The capability response includes `protocol`, `protocol_version`,
`server_version`, `actions`, `action_descriptors`, `resources`, `limits`,
`scopes` and optionally server and artifact identities. Descriptors contain
per-action input/output JSON schemas. Implementations and clients MUST tolerate
optional capability fields absent from older peers; an unknown action MUST NOT
be invoked merely because its identifier is recognizable.

| Method | Path under `/control/v1` | Purpose |
| --- | --- | --- |
| GET | `/capabilities` | Caller-specific discovery |
| GET | `/state` | Reload generation and configuration state when enabled |
| GET | `/status`, `/models`, `/requests` | Status, models and request reports |
| POST | `/route/preview` | Preview a route |
| GET | `/providers`, `/observe/status`, `/policy/status`, `/policy/show`, `/agents` | Administration reports when enabled |
| POST | `/reload` | Submit guarded reload when enabled |
| GET | `/operations/{request_id}?instance={server_instance_id}` | Read the caller's reload operation |

`models` accepts the optional `provider` query. Filter and payload shapes are
provided by discovery and the [public types](../../../crates/bitrouter-ai/src/control).
An endpoint's presence in this table does not guarantee it is enabled in a
particular distribution or granted to a caller.

### Authentication and isolation

Credentials are bearer tokens in `Authorization`. `control:read` grants
inspection; `control:reload` authorizes reload and requires `control:read`;
`control:tenancy` is separate and does not imply either. Host control tokens
are independent of inference credentials. The default legacy token grants read
only. The listener's deployment and TLS requirements are operator settings;
clients MUST authenticate the target and MUST NOT send tokens to an untrusted
host. Token configuration requires at least 32 UTF-8 bytes. Handlers recheck
scope even if discovery advertised an action.

### Guarded reload

A client reads state, then submits JSON with exactly `request_id` (UUID),
`expected_server_instance_id` (UUID) and `expected_generation` (integer).
For example, after reading the actual values from state, a request body has
this shape (the UUIDs are synthetic):

```json
{
  "request_id": "7b2bb8ba-d421-4cff-ab0a-a9e311e3f834",
  "expected_server_instance_id": "81ba8ac5-d17b-4b41-8fca-1d30e8802a46",
  "expected_generation": 12
}
```

The request asks the server to reload its own saved configuration; it does not
upload or freeze a configuration snapshot. A successful admission returns HTTP
202 with an operation report and relative lookup URL. Identical requests by the
same credential and ID return the existing operation; changing the input for
that ID returns `409 idempotency_conflict`. A different boot, stale generation,
or an already-running reload is rejected without admitting a new reload.

The daemon owns admitted work after disconnect. Operation status is `running`,
`succeeded`, `failed`, `partially_applied` or `unknown`; partial/unknown is not
success. Reports identify participant results and do not promise a transaction
across subsystems. Completed results are retained in boot-local memory for 24
hours, with at most 1,024 operations. A restart or expiry can make the outcome
unknown. A client MUST NOT infer rollback from a timeout, expiry or lost daemon;
it SHOULD inspect live state before initiating a different operation. Do not
resolve a lookup URL outside the same origin.

### Errors and evolution

Control errors have `{"error":{"code":"...","message":"..."}}`.
Examples include 401 `unauthorized`, 403 `scope_denied`, 400
`invalid_request`, 404 `unsupported_action` or `operation_not_found`, 409
`stale_generation`, `server_instance_changed`, `reload_in_progress` or
`idempotency_conflict`, 410 `operation_expired`, and 503 `operation_capacity`.
Clients MUST handle unknown codes as failures and MUST NOT retry mutations
blindly. Inputs with unknown fields are rejected where the input schema says so;
clients SHOULD ignore unrecognized response fields and consult advertised
schemas. No extension or version negotiation beyond discovery is promised.

### Control API compatibility cases

These are observable cases for the beta contract, not a report of a particular
build. Enable the relevant host features and use independent read-only and
read-plus-reload credentials.

1. A missing or invalid bearer credential cannot read capabilities. Read-only
   discovery omits reload, and a direct reload request is denied.
2. Discovery identifies only enabled and authorized actions and supplies their
   request and response schemas. A client handles a missing optional descriptor.
3. Submit a valid reload using the sampled instance/generation. Disconnect and
   look up the same request ID on the same boot; the operation continues.
4. Replay the identical body under the same credential and verify no second
   reload is admitted. Reuse the ID with changed input: 409 conflict.
5. Stale generation, another boot and concurrent reload do not start a second
   operation. Unknown/expired results are not represented as a confirmed rollback.
6. A partially applied operation is a terminal report with participant outcomes,
   not a successful atomic transaction. Unknown response fields/codes do not
   make a client report success.

## Tenancy API

### Transport and authority

The optional tenancy service uses the protected control listener. Authentication
uses one `Authorization: Bearer <token>` header; `x-api-key`, duplicate
security-relevant headers and content encoding are rejected. Installation
operators use a host-control credential with `control:tenancy`; tenant
credentials derive their authority from their current binding and grant. Neither
an identifier nor the name of a tenant grants authority. Effective access is
the intersection of credential, current binding, scope, expiry and tenant
lifecycle. A suspended tenant and a revoked or narrowed grant cannot authorize
new work. Control credentials and inference credentials are distinct.

Authenticated `GET /control/v1/tenancy/capabilities` discovers installation
identity and capabilities. Other GETs require `Bitrouter-Installation-Id`;
mutations carry `installation_id` in their typed body. A mismatching identity
is rejected rather than retargeted. The server checks Origin/Host where Origin is
present. Capability discovery does not itself enable dispatch or prove that an
optional backend or feature is available. Tenancy is opt-in; the currently
qualified local dispatch profile requires a Unix owning-process SQLite service
and remains subject to live authority, storage and admission fences. Other
backends or platforms MUST NOT be inferred from the DTOs alone.

### Operations

All paths below are relative to `/control/v1`. Payload structures and field
limits are defined by the [JSON Schema](../../contracts/resource-tenancy/schema.json)
and the [public DTOs](../../../crates/bitrouter-ai/src/tenancy). A schema-valid
request may still fail semantic, authorization or capability checks. The
[fixture set](../../contracts/resource-tenancy/fixtures.json) includes shape and
semantic negative cases.

| Method | Path | Result |
| --- | --- | --- |
| GET | `/tenancy/capabilities` | Capabilities and installation identity |
| GET | `/tenancy/directory` | Operator-only, revision-bound directory page |
| PUT | `/tenants/{tenant_id}`; `/principals/{principal_id}` | Tenant or principal operation result |
| PUT | `/tenants/{tenant_id}/bindings/{binding_id}` | Binding operation result |
| DELETE | `/tenants/{tenant_id}/bindings/{binding_id}/management` | Detach managed writer |
| POST | `/tenants/{tenant_id}/credentials`; `/tenants/{tenant_id}/audit-readers` | Issued credential; secret only on initial issuance |
| GET, DELETE | `/tenants/{tenant_id}/credentials/{credential_id}` | Status or revoke |
| POST | `/tenants/{tenant_id}/configurations` | Immutable configuration revision |
| GET | `/tenants/{tenant_id}/configurations/{revision_id}` | Configuration revision |
| GET, POST | `/tenants/{tenant_id}/activation` | Active revision or guarded activation |
| GET | `/tenancy/operations/{operation_id}` | Authorized operation result |
| GET | `/tenants/{tenant_id}/executions/{execution_id}` | Execution receipt |
| GET | `/tenants/{tenant_id}/executions` | Receipt page; `limit` 1–100 |

A dedicated audit reader has receipt-read authority, not general tenant access.
Configuration activation requires tenant-wide policy activation authority;
resource-scoped access alone does not authorize whole-tenant activation.

### Operator directory readback

`GET /control/v1/tenancy/directory` requires an installation operator with
`control:tenancy` and `Bitrouter-Installation-Id` on every page. Tenant
credentials, including tenant administrators and audit readers, and host
`control:read` credentials cannot enumerate the directory. All responses,
including failures, carry `Cache-Control: no-store`.

[Directory DTOs](../../../crates/bitrouter-ai/src/tenancy/directory.rs) and the
[directory JSON Schema](directory.schema.json) define `DirectoryPageV1` and its
`kind`-tagged `DirectoryEntryV1` union. Entries expose tenant lifecycle,
principal issuer/subject, binding grants and management ownership, and durable
credential metadata. Credential history includes revoked and expired records
even when they are absent from the live authentication map. `recorded_state`
preserves the stored value; readers derive expiry from `expires_at` at their
observation time. Neither a recorded `active` state nor a directory entry grants
authorization. Existing audit readers have `binding_id: null`, `permissions: []`
and `audit_reader: true`. Detached bindings have `management_owner: null`.
Bearer tokens, credential hashes, ciphertext, upstream secrets, resource
plaintext and operation secrets are never returned. Diagnostics omit contents.

`limit` defaults to 100 and must be an integer in 1–100. Each page contains at
most that many entries and at most 1 MiB of serialized JSON, including the
envelope and cursor. The byte bound can produce a shorter page. Empty results
are successful with `entries: []` and `next_after: null`; a full final page also
has a null continuation. A record that cannot fit by itself fails with
`payload_too_large`, without skipping it.

Ordering is ascending ASCII tuple order `(kind, tenant_id, id)`: `binding`,
`credential`, `principal`, then `tenant`. Bindings and credentials use their
tenant ID and their own ID. Principals and tenants use an empty tenant ordering
component and their own ID. IDs retain case and punctuation; no locale or
case folding applies. `after` is an exclusive cursor, encoded exactly as
`v1|kind|tenant_id|id`, with those same ordering components. For example,
`v1|binding|tenant-a|binding-a` and `v1|principal||principal-a` are valid.
Percent-encode the cursor as an ordinary query parameter. It is a position,
not a credential or a snapshot lease.

An initial request omits `after` and normally omits `authority_revision`; its
response establishes the scan revision. Every continuation MUST supply that
revision with `after`. A cursor without a revision, malformed cursor, unknown
query field, duplicate field or invalid limit returns 400 `invalid_request`.
Every requested revision must match current authority or return 409
`revision_conflict`; the service never returns a mixed-revision scan. On
conflict discard the incomplete scan and start again explicitly. Installation
mismatch remains 409 `capability_mismatch`. Reauthentication and live authority
checks occur independently for every page. There is no automatic retry.

For example, a one-tenant store can return:

```json
{"schema_version":1,"installation_id":"installation-a","authority_revision":1,"entries":[{"kind":"tenant","tenant_id":"tenant-a","revision_id":"revision-a","state":"active"}],"next_after":null}
```

The additive SDK method is
`TenancyClient::directory(installation, authority_revision, after, limit)`.
It validates installation, requested revision, ordering, continuation and count,
and caps received bytes at 1 MiB or the client's lower configured response bound.
The DTO rejects unknown fields and variants; clients must fail visibly on
unsupported schema versions. Existing clients and other tenancy DTOs are
unchanged; older servers may return 404 for this additive endpoint.

Compatibility checks cover empty and exact full pages, ASCII pagination without
duplicates or omissions, intervening mutations, wrong installation and malformed
cursors, current operator authority on continuation, durable revoked/expired
credentials after restart, detached ownership, redacted diagnostics, and byte
truncation without skipping an entry. Existing mutation CAS and managed-owner
fences continue to govern any repair based on these observations.

### Roles, scope and version binding

The fixed tenant roles are `viewer` (`resource.read`, `receipt.read`),
`editor` (viewer plus `resource.draft`, `execution.submit`, `credential.use`,
`evaluation.submit`), `tenant_operator` (viewer plus `policy.activate`,
`credential.manage`) and `tenant_admin` (the union plus `binding.manage`,
`credential.issue`). Editor and tenant operator are not a hierarchy. A tenant
operator role or credential management alone does not grant inference.

A grant is explicitly tenant-wide or names a nonempty unique resource set;
an empty set is not a wildcard. Operator pools require explicit live grants and
are not implied by tenant-wide scope. Delegation and credential issuance cannot
exceed the issuer's effective permissions, resource scope or expiry. Binding
updates and credential revoke compare the observed revision. A tenant-wide
activation requires tenant-wide `policy.activate` in both the current credential
and binding. Current permission and lifecycle are checked again at the point
of an attempt; a previously admitted configuration is not a permanent grant.

The configuration and its referenced route/policy revisions are pinned for
execution. Current health, credential and authority changes can narrow eligible
targets, not expand an admitted envelope. Suspended or deleting tenants cannot
start or wake work; already admitted work can still finalize under its existing
ownership. A receipt-only audit reader may inspect retained receipts without
gaining mutation or execution authority.

### Strict tenant dispatch binding

Tenant inference uses the inference listener with a tenant bearer credential,
separately from management on the control listener. The
[public `DispatchBinding` DTO and validator](../../../crates/bitrouter-ai/src/tenancy/operation.rs)
and its definition in the [tenancy schema](../../contracts/resource-tenancy/schema.json)
are the catalog for the seven required HTTP request headers. Each header MUST
occur exactly once with a valid value; omitted, duplicate or malformed binding
headers are rejected. Header names follow HTTP case-insensitivity. Version is
the literal string `1` and receipt mode is `strict`; there is no implicit
best-effort fallback. Caller-supplied `Bitrouter-Tenant-Id`,
`Bitrouter-Principal-Id` and `x-api-key` are rejected. Tenant and principal
identity come from the authenticated credential and current authority.

A binding uses an explicitly observed tenancy descriptor with
`revision_bound_dispatch`, `strict_receipts` and `dispatch_enabled` all true.
It addresses the persistent installation, that process incarnation and the
observed capability revision, together with an explicitly selected immutable
execution revision and caller correlation ID. The server rechecks binding,
lifecycle, scope and storage/admission eligibility, including after waits and
before attempts. A restart, stale capability revision or disabled dispatch
returns `capability_mismatch`; malformed binding returns `invalid_request`.
Other current-authority failures retain the tenancy error mappings below.
Discovery is no guarantee that a later submission will be admitted. A queued
submission retains its selected revision when activation changes, without
extending credential lifetime or gaining new targets.

Each admitted invocation has a fresh execution ID. For this tenant profile,
the response `x-bitrouter-request-id` contains that execution ID, not the
submitted correlation ID. It can be absent on preadmission failure and MUST NOT
be reconstructed from provider output. This differs from the
[shared gateway identity profile](#shared-request-identity); its separate
idempotency/202 behavior does not apply here. Repeating a correlation ID or
reusing a dispatch binding does not deduplicate inference.

The [scoped SDK adapter](../../../crates/bitrouter-sdk/src/gateway/tenancy.rs)
validates the binding before sending one request, without automatic discovery,
capability refresh, retry, replay or remote cancellation. Chat Completions,
Messages and Responses use JSON/SSE; Decisions uses JSON. The adapter requires
HTTP 200, a single valid execution ID and the expected representation for model
output; 202 is not model success. On a tenancy error, the error's execution ID
must agree with response metadata, its operation ID must be null and its code
must match the HTTP mapping. Invalid metadata, malformed/oversized bodies,
incomplete streams or a transport failure leave dispatch outcome unknown rather
than trusting an apparent `not_started` claim. A validated `not_started` refusal
describes provider-dispatch evidence, not an automatic resend instruction.

Strict connected JSON success and the final successful SSE delivery signal
wait for durable receipt publication. Earlier output or finish metadata is not
proof of that commit. Dropping or timing out a client stream stops local
consumption; it does not prove provider cancellation, receipt completion or safe
replay. Clients MUST retain observed execution identity and query its receipt
with current authority after uncertainty. A missing receipt is not proof that
no provider work ran. No stream failure, lost response or capability refresh
authorizes automatic resubmission of the uncertain invocation.

### Identity, replay and failures

An operation ID is bound to installation, authenticated principal and typed
command. An exact authorized replay returns the recorded result; reuse for a
changed command conflicts. Current authorization is rechecked before revealing
a recorded result. Credential issuance returns a secret only in the initial
successful response; replay and lookup MUST NOT reveal it. On a lost issuance
response, recover the ID/status and revoke or rotate as appropriate. Correlation
IDs are not idempotency keys. The result lookup/replay window is 30 days; after
expiry the operation ID remains permanently reserved. The local format-2 store
retains its indexed operation record and does not impose a lifetime operation
count quota or recycle IDs. Active work, queues, reads and storage admission
remain bounded by the startup host budgets. A principal can recover its own
unexpired operation only while current authority covers its saved scope.

Revision beta.3 clarifies those capacity and retention semantics without
changing strict-v1 message shapes or per-object limits. Host budgets are defined
by the [configuration contract](../configuration/README.md#process-capacity).
The frozen [version-1 capacity profile](../../contracts/resource-tenancy/benchmark-profile.json)
remains a bounded artifact profile; its legacy catalog and operation counts are
not format-2 host quotas.
A persisted legacy saturation latch is not silently cleared by a new binary.
The verified local upgrade is an operator storage transition, not an HTTP
resume operation or permission to repeat an uncertain command.

The tenancy error body is a JSON object with `code`, `message`, `dispatch_state`,
`operation_id` and `execution_id` (the last two may be null). HTTP mappings are:
400 `invalid_request`; 401 `unauthorized`; 403 `forbidden` or
`tenant_inactive`; 404 `not_found`; 409 `revision_conflict`,
`operation_conflict` or `capability_mismatch`; 410 `operation_expired`; 413
`payload_too_large`; 429 `capacity_exceeded`; and 503 `authority_unavailable`
or `storage_unavailable`. `dispatch_state` distinguishes `not_started`,
`started` and `unknown`. A transport timeout is not proof of rollback or that
provider work did not start. Clients MUST treat unrecognized codes and states
as failures and MUST NOT automatically repeat an uncertain execution.

### Receipts and pagination

A receipt identifies an installation/execution and its actual attempts,
configuration and available usage evidence. Unknown quantities remain null;
an estimate or missing attempt cannot be promoted to complete provider usage.
The receipt endpoint is tenant-wide read or dedicated audit-reader authority.
Correlation queries return an `ExecutionPage` ordered by execution ID, with an
exclusive `after` cursor; `next_after` is null on the last page and otherwise
is the last returned ID. `multiple_executions` applies to the full matching
set, not just one page. Newly observed executions can require restarting the
query; clients deduplicate by installation and execution IDs. A missing receipt
is not proof that provider work never ran. Strict receipt mode and ordinary
best-effort recording have different durability guarantees.

Archiving an eligible terminal receipt is an internal storage relocation. It
MUST preserve execution identity, exact receipt/evidence bytes and digests,
authorization, correlation pagination and original-operation identity rules.
Inline and archived receipts use the same public read APIs. Archive movement
MUST NOT discharge an unresolved obligation or authorize redispatch. The local
store has no history-row lifetime quota or automatic evidence GC; physical
storage and bounded request/response profiles still apply.

### Evolution and conformance

The schema's own `additionalProperties` rules govern requests and responses.
Strict tenancy capabilities, receipts and the resource/evidence envelopes below
reject unknown or duplicate fields and unknown enum values. They are not open
extension points. Required nullable fields MUST be present: omission is not
equivalent to null. Unsupported versions, inexact integers and out-of-bound
values fail validation. No stable compatibility guarantee is claimed in beta.

An old client can continue using the unchanged tenancy discovery and receipt
messages against a new server. A new client MUST discover resource and evidence
support separately; the original tenancy capability document does not advertise
them. An older server may lack those endpoints. An absent endpoint or an explicit
disabled capability means the facility is unavailable, not permission to send
new fields to an old endpoint. Authentication, authorization, transport and
malformed-response failures MUST NOT be treated as successful discovery or as
historical absence of evidence. Clients may continue an explicitly optional
receipt-only workflow; required provisioning or evidence must fail visibly.

### Resource provisioning

Resource provisioning creates immutable upstream credential, route and policy
revisions. These are service messages within this specification, not a separate
feature specification. It defines no login, commercial account, pricing or
settlement contract. The qualified deployment remains opt-in, exclusively owned
Unix SQLite in a single process; these guarantees do not establish distributed
ownership or safety. Assembly and operational boundaries are in the
[tenant service guide](../../development/tenant-service.md).

All paths below are relative to `/control/v1` and use the tenancy authentication,
installation binding, current authority, errors and no-store response policy.

| Method | Path | Contract |
| --- | --- | --- |
| GET | `/tenancy/resource-capabilities` | [Resource capabilities schema](../resource-provisioning-v1/capabilities.schema.json) |
| POST | `/tenants/{tenant_id}/resources` | [Put command schema](../resource-provisioning-v1/put.schema.json); existing tenancy `OperationResult` |
| POST | `/tenants/{tenant_id}/resources/revoke` | [Revoke command schema](../resource-provisioning-v1/revoke.schema.json); existing tenancy `OperationResult` |
| GET | `/tenants/{tenant_id}/resources` | Exact `ResourceRef` query; [status schema](../resource-provisioning-v1/status.schema.json) |

These schemas remain at their build/test-consumed paths and are the field
catalog. The [DTOs and validator](../../../crates/bitrouter-ai/src/tenancy/resources.rs)
also enforce semantic constraints. Discovery requires `Bitrouter-Installation-Id`
and reports provisioning availability and command bounds; it grants no authority.
Without a configured wrapping key, `provisioning_enabled` is false. Upstream
secrets are distinct from the bearer credentials used to call this API.

#### Creation, rotation and authority

For example, this synthetic request creates a managed credential head:

```json
{
  "format": 1,
  "operation_id": "provision-slot-01",
  "installation_id": "example-installation",
  "key": {
    "owner_kind": "tenant",
    "owner_id": "example-tenant",
    "resource_kind": "credential",
    "resource_id": "upstream-slot"
  },
  "expected_revision": null,
  "definition": { "kind": "credential", "secret": "synthetic-example-only" }
}
```

The required `expected_revision` is null only for creation of a remotely managed
head; otherwise it MUST equal that head. The service assigns a new immutable
revision. Rotation MUST NOT rewrite old definitions or retarget configurations
that selected them. A manifest-only resource has no managed head; a managed
create of the same key leaves its existing manifest revisions unchanged.
Resource identity includes owner kind, owner ID, resource kind and resource ID;
an exact reference additionally includes revision ID. Authority revision and
content revision are distinct and MUST NOT be substituted for each other.

The same format-one command has two additional closed definition variants:
`admission_profile` contains an `AdmissionProfileV1`, and `profiled_route`
contains the existing concrete routing fields plus a total attachment map.
An admission profile is an operator-pool Policy resource, distinct from a
PolicyLock definition. Both variants require installation authority, including
a profiled route with a tenant owner. A profile key cannot change between the
two Policy definition types. Only its member RPM ceiling may rotate; rate-group
membership, health rules and fallback remain immutable for that key. Zero RPM
is valid and means local refusal, while explicit null disables a gate.

Attachments identify exact ingress protocol, original candidate ordinal,
credential revision and profile revision. Compilation verifies total, unique
coverage of the three retained generation roots. Policy, continuation, counting,
dynamic credential and other unsupported roots cannot acquire a profiled
definition. All existing command, import, artifact and configuration byte bounds
apply to the final serialized objects; cardinality ceilings do not promise that
every combination of maximum-width values fits.

Accepting a profiled route permanently protects its resource key across retained
revisions. Legacy activation or execution cannot bypass that protection by using
an old configuration, default route, native entrypoint or auxiliary operation.
Revocation and rotation retain the old immutable definitions and protection.
Read authority and existing terminal obligations remain independent of current
operational eligibility. The private format-one import accepts the same new
variants. It initializes one unambiguous head for a new profile key atomically;
identical reimport never rewinds the head. Later rotations use authenticated CAS.
Missing retained profile heads are errors, not disabled admission rules.

Credential writes require `credential.manage`; route and policy writes require
`resource.draft` on the addressed key. Every dependency in the compiled closure
requires current `resource.read`; credential dependencies also require
`credential.use`. A resource kind MUST match its definition. Tenant credentials
MUST NOT mutate operator-pool resources; operators MUST NOT place another
tenant's private dependencies into a tenant's configuration. Same-name private
resources do not share authority.

Commands are bounded to 1 MiB including the envelope; credential secrets are
1–65,536 UTF-8 bytes. Duplicate provider/account bindings or dependencies, inline
provider secrets, ambient provider discovery, host configuration and invalid
closures are rejected. Binding/dependency limits come from the schemas. The
embedded routing and policy objects are interpreted by the service compiler,
not arbitrary extension bags: schema-valid objects can still be rejected for
unknown required behavior, invalid references or unavailable binary features.
Their applicable configuration semantics are in the
[configuration specification](../configuration/README.md).

Tenant-authored routes MUST use operator-approved exact HTTP(S) origins.
They cannot override destination authority through Host, authentication,
forwarding or hop-by-hop headers; permitted explicit headers are limited to
`anthropic-version`, `anthropic-beta`, `openai-beta` and `accept`.
Operator-authored routes retain explicit host upstream authority. Operators own
the approved origins and their DNS/network destinations. Approval qualifies the
retained revision: changing the startup origin list does not revoke it.
Withdrawing approval requires explicit revocation of affected revisions.

#### Revocation, reads and replay

Revocation addresses an exact reference and compares
`expected_authority_revision` with current authority. It requires
`credential.manage` for credentials or `policy.activate` for other resources.
Both old and current revisions can be revoked. Revocation MUST fence subsequent
use, including cached dispatch and new compilation/activation, without erasing
immutable evidence. Re-enabling requires a new revision and explicit
configuration selection. A revoke cannot undo an upstream request already sent.

Status reads use the exact reference as query parameters and recheck current
`resource.read` authority before storage access and before returning data.
The required nullable `head_revision` is null for a manifest-only key; it is not
an instruction to resolve a different revision. Status exposes metadata, never
definitions or secrets. Secrets MUST be encrypted at rest with installation and
reference binding, and absent from operation records, diagnostics and errors.
An unkeyed digest of secret-bearing commands MUST NOT be retained as their
fingerprint.

Resource bytes, head/revocation metadata and operation outcomes MUST commit
atomically before acknowledgement and authority publication. Identical
authorized replay under the same principal and operation ID returns the original
result; changed content returns `operation_conflict`. CAS failures use
`revision_conflict`; unavailable provisioning uses `capability_mismatch`.
Other failures use the existing tenancy error vocabulary and HTTP mappings.
After a lost reply, query `/tenancy/operations/{operation_id}` using the original
ID. Handler cancellation MUST NOT lose admitted work or create another writer.
An unresolved outcome stays unknown; it does not authorize a replacement
operation ID or inference replay. Clients MUST bound response decoding, bind
requests to the installation and check returned operation/resource identities
before accepting success. Capability and status installation IDs MUST match the
requested installation; an `OperationResult` does not add an installation field.

### Ordered static dispatch v2

Ordered dispatch is an explicit mode on the existing `/v1/chat/completions`,
`/v1/messages` and `/v1/responses` endpoints. It uses the same protocol bodies,
JSON/SSE rendering and current-authority send gates. It does not add inference
idempotency, a retry API, a scheduler callback or a separate dispatcher.

| Method | Path under `/control/v1` | Purpose |
| --- | --- | --- |
| GET | `/tenancy/ordered/capabilities` | Authenticated v2 discovery before installation binding |
| GET | `/tenancy/tenants/{tenant}/configurations/{revision}/candidates` | Operator-only descriptors for an exact retained protocol root |
| GET | `/tenancy/tenants/{tenant}/executions/{execution}/v2` | Explicit v2 receipt read with current receipt authority |
| GET | `/tenancy/tenants/{tenant}/executions/{execution}/usage-evidence/v2` | V2 original usage observations with the same authority |

The [ordered dispatch schema](ordered-dispatch.schema.json) and the public
[selection](../../../crates/bitrouter-ai/src/tenancy/ordered.rs),
[receipt](../../../crates/bitrouter-ai/src/tenancy/execution_v2.rs) and
[evidence](../../../crates/bitrouter-ai/src/tenancy/usage_evidence_v2.rs) validators
define required fields, nullability, versions and semantic constraints. All
reads other than discovery require `Bitrouter-Installation-Id`; protected
responses use `Cache-Control: no-store`.

#### Discovery, selection and authority

`OrderedCapabilitiesV2` reports contract version 2, installation/instance IDs,
capability and authority revisions, the host attempt limit, and the three
dispatch/receipt eligibility flags. A client MUST explicitly require all three
flags and respect the reported limit. Discovery does not grant resource access.
The server rechecks instance, capability, configuration and current grants at
admission and every actual send. A restart invalidates the prior instance binding.

Manifest queries require exactly `route_owner_kind`, `route_owner_id`, `route_id`,
`route_revision` and `inbound_protocol`. Ordinals identify positions in the
persisted protocol root, not positions in a private deduplicated target array.
The manifest contains at most 256 entries and 1 MiB of JSON, with each ordinal,
nonsecret configured target IDs and exact credential resource reference.
Its required `admission_mode` is `legacy` or `profiled_v1`; each descriptor has
an explicit nullable `admission_profile`. Profiled descriptors must carry the
exact pinned profile reference, including for explicitly disabled gates. The
required-nullable `runtime_observation` is advisory process-local information,
with one entry per descriptor in the same ordinal order. Missing profile
authority is unavailable/denied, never an invented zero count. Producers use
whole-second UTC timestamps and validate the complete manifest after appending
observations. No observation grants admission or proves provider work.
All candidates in one manifest share one bounded read snapshot of the local
rate and health state; concurrent admission cannot split a shared group's count
inside that response. A later actual hop still performs its own admission.

An operator profile may include `observe_rate_group_id` for a read-only group
in the profile's own operator-pool namespace. This association is immutable
across revisions. If `rate` is non-null, its group must match the observed group.
The runtime observation then includes `observed_shared_count`, without inventing
a ceiling for a `rate:null` profile. Reading or executing such a rate-disabled
member does not charge that group or consult another member's health. Its own
explicit health/fallback rules remain independent. An authorized, valid group
with no live timestamps has measured count 0 for this process instance; absent
association, missing profile authority or unavailable state has no count.
Counters include the same group across retained roots, not only candidates in
the queried manifest. Optional observation fields are omitted when unused.
It contains no credential bytes, URL, arbitrary metadata or provider body.
An authority change during read fails rather than mixing revisions. Tenant
inference credentials cannot enumerate this operator view.

The admission mode is mandatory in selections and accepted selections; it has
no deserialization default. Previously retained ordered bindings or receipts
without the field require an explicit historical decoder in archive consumers.
Preserve their original bytes and digests instead of reserializing them through
the current DTO or injecting a field. A historical read projection grants no
new dispatch authority. Previously retained manifests also lack the new
required-nullable fields. Legacy v1 receipt and evidence formats are unchanged.

A v2 request sends the existing seven strict binding headers with
`Bitrouter-Tenancy-Version: 2`, plus exactly one `Bitrouter-Candidate-Selection`.
The latter is strict JSON encoded as canonical, unpadded RFC 4648 base64url,
at most 4096 encoded bytes. Its decoded shape is:

```json
{
  "schema_version": 1,
  "admission_mode": "legacy",
  "route": {
    "owner_kind": "tenant",
    "owner_id": "example-tenant",
    "resource_kind": "route",
    "resource_id": "example-route",
    "revision_id": "route-revision"
  },
  "candidate_indices": [3, 1]
}
```

The route MUST match the exact retained revision and body model. The ordered
mode is required and must match the authenticated retained root; omission is
not a legacy default. Predecessor closed readers reject the added field, while
current readers reject the old missing-field shape. This completes the
unreleased ordered contract without changing strict v1 bytes or endpoints. The
accepted mode remains part of selection identity in the existing v2 receipt,
usage evidence and bounded dispatch intent. The ordered
list has 1–64 unique integer ordinals below 256, each present in that root.
In legacy mode, different ordinals that resolve to the same private candidate
cannot both be selected. Profiled mode permits equal private candidates at
distinct original ordinals, preserving each ordinal's own profile attachment
and reached-hop admission. Duplicate ordinals are rejected in both modes.
Empty, unknown, oversized or unauthorized selections also fail before sending.
V1 requests carrying the selection header are rejected; a
client MUST NOT retry an unsupported v2 request as v1 or truncate its list.
The header is removed before provider dispatch.

Selection constructs an immutable request-local view. It narrows candidate
membership and required credential dependencies without changing the cached
revision, active configuration or another request. It never grants permission
to a selected resource. Only policy-free static generation roots without
learning, experiments, auxiliary counting, native harness, OAuth refresh or
continuation requirements qualify. Continuation-bearing input fails explicitly.
Unselected private credentials do not become usable through later hooks.

The selected order bypasses runtime sorting and weighted permutation. Existing
live rate/health admission and fallback classification still apply. A permitted
fallback can skip an ineligible candidate; actual attempts form a unique ordered
subsequence. Unknown transport/body/stream outcome, cancellation or completed
work MUST stop further attempts. Missing usage dimensions alone do not imply an
unknown execution outcome. All attempts share the admitted execution deadline;
fallback does not reset it or the separate stream-idle rules.

#### Receipts, bounds and recovery

V2 receipts explicitly bind the accepted route, server-observed ingress protocol,
ordered selection and host attempt limit. Every actual attempt records its
selected ordinal and a required nullable observed HTTP status (200–599).
Completed attempts cannot claim non-2xx status. An observed 2xx status does not
prove completion. An actual unknown attempt requires an unknown receipt;
successful execution with missing usage can still be `incomplete`.

V2 receipt JSON is bounded to 1 MiB. Let A be accepted selection length and
K = ceil(A / 8). Original usage evidence allows 512K frames, at most 128 per
actual attempt and 32 quantities per frame. Frame bytes are bounded to 192 KiB K,
with 64 KiB K reserved for identities and terminal metadata, for at most
256 KiB K total (2 MiB at A=64). This allocation depends on accepted candidates,
not eventual sends. Overflow retains a prefix and sticky truncation; it never
manufactures zero quantities or complete evidence. V2 has no auxiliary children.

Before any effectful provider work, the owning writer commits a bounded dispatch
intent containing the immutable identity, selection and budgets. An uncertain
intent acknowledgement cannot authorize a send. Terminal receipt, original
evidence and intent retirement commit atomically. Startup terminalizes surviving
intents as unknown with unavailable observations before enabling dispatch; it
never replays inference. A missing receipt is not proof that no work happened.
The maximum receipt/evidence/disposition/intent obligation, including three row
charges, is 3,212,288 bytes and fits the existing 4 MiB execution reservation.
Ordinary v1 requests retain their original eight-slot allocation and budgets.

V1 and v2 reads are explicit: requesting the wrong format returns
`capability_mismatch`, with no conversion, flattening or truncation. Existing
correlation pagination can list both formats and retains multiplicity. Neither
an execution ID nor a correlation ID expands receipt-read authority.

Fresh stores and explicitly migrated stores use authority format 3. Only
released format-one stores migrate; unreleased format-two stores are refused
without implicit conversion. Profiled configuration envelopes use format 4
and an authenticated admission compiler subtype; retained formats 1–3 remain
readable under their original compiler bindings. Every
management write preserves that marker. An older binary MUST refuse the store,
including after fresh initialization or a later management mutation. Existing
v1 receipt bytes and integrity digests remain unchanged. Rollback requires the
stopped-writer, compatible-backup procedure below; deleting v2 records or
resetting a version marker is not a supported downgrade.

#### Scoped Beta header preservation

Ordered generation can opt into one closed per-call default object described by
[`generation-defaults.schema.json`](generation-defaults.schema.json). Its only
fields besides `schema_version: 1` are `temperature`, `top_p`, `max_tokens`,
`reasoning_effort`, `top_k`, `seed`, `stop`, `presence_penalty`, and
`frequency_penalty`. Absent fields supply no default. The object rejects null,
unknown or duplicate fields, non-finite numbers and invalid typed values. A
single stop string normalizes to an array. No provider-specific extras, model,
credential, header or routing control can be carried by this object.

Call `OrderedDispatchBindingV2::with_generation_defaults` before retaining the
submission binding, then supply the same object through the consuming
`OrderedTenantGateway::with_generation_defaults` method. The optional
`generation_defaults_digest` is SHA-256 of the ASCII domain
`bitrouter.canonical-generation-defaults.v1` followed by a zero byte and the
compact typed JSON in declared field order. Absent fields are omitted. It binds
the normalized requested defaults, not the raw request, effective provider
settings or usage. Selection, accepted selection, intent, receipt and evidence
retain that digest. The SDK rejects a missing, changed or competing object
before I/O; the service repeats validation before execution admission.

The SDK carries the object in `bitrouter_generation_defaults` in the existing
request body. Core rejects duplicate reserved members, removes the object before
protocol parsing, and never forwards it. The whole request remains limited to
16 MiB; the fixed digest retains the existing 4,096-byte encoded selection
limit. Unused extensions omit both members and preserve previous bytes. An old
closed selection reader rejects the used extension. This optional field does
not relax the separately mandatory `admission_mode` contract.

Defaults fill the canonical prompt before retained configuration defaults and
normal policy checks. Typed caller values, including zero, win. Typed null
becomes `None` and permits filling; an empty stop list also permits filling.
An opaque source extra with the competing canonical name blocks filling without
being promoted across protocols. Existing outbound adapters control supported
spellings and omissions on each fallback hop. Strict v1, native Decisions and
host execution do not gain this per-call API.

The scoped v1 and ordered v2 SDK helpers provide the consuming
`with_anthropic_beta(&[reqwest::header::HeaderValue])` method. It replaces only
the current call's `anthropic-beta` fields. Values retain their bytes, duplicate
entries and order; comma lists are not split, trimmed, sorted or deduplicated.
An empty slice removes the header, while an explicitly empty field value stays
present. Invalid HTTP field-value bytes are rejected by `HeaderValue`
construction before any request. Shared client defaults and dispatch bindings
remain unchanged. Preservation applies to parsed field values, not header-name
casing, HTTP whitespace framing or HTTP/2 frame layout.

The SDK sends these values to the gateway independently of the ingress
protocol. The existing service policy forwards them only to an actual Messages
upstream; provider authentication may subsequently merge required Beta tokens.
The method does not override the fixed Anthropic version or expose arbitrary
headers. This is an additive SDK operation using existing wire behavior, with
no dispatch, selection or receipt format change and no automatic retry.

#### Scoped response metadata

The scoped SDK preserves a single `Retry-After` decimal seconds value when it
contains 1–20 ASCII digits and fits `u64`, including zero. Missing, duplicate,
HTTP-date or invalid/overflow values remain absent. The sole recognized error
source is the singleton `X-Bitrouter-Error-Source: upstream`; absence is not
proof of a local refusal. Invalid optional metadata does not erase a valid
execution ID or status. These observations do not cause automatic retries.

#### Ordered compatibility cases

1. Old v1 clients retain their wire contracts; new clients reject unsupported
   v2 servers before any downgrade or alternative model submission.
2. Different concurrent subsets/orders of the same retained root preserve
   their own membership, resource authorization and immutable ordinal mapping.
3. Counts 1, 8, 9 and 64 retain all actual attempts; 65 and a host limit below
   the submitted count reject before provider work.
4. Current revocation after queueing or between hops blocks the next send;
   selection and capability caches never stand in for current grants.
5. JSON and SSE for all three protocols preserve their terminals and original
   usage through HTTP/1.1 and HTTP/2; wrong-format reads fail explicitly.
6. Unknown outcomes, truncated observations, lost intent acknowledgement and
   crash recovery cannot authorize replay, a second attempt or fabricated usage.
7. Fresh/migrated format-2 stores, including subsequent management writes,
   refuse an old binary; historical v1 receipt bytes remain readable unchanged.
8. Repeated opaque Beta values remain isolated per call over HTTP/1.1 and HTTP/2
   for JSON and SSE; replacing or clearing them cannot change the binding,
   request body or another call's headers.

### Execution usage evidence

Usage evidence contains bounded technical observations, not prices, charges or
payable quantities. It is a separate message from the unchanged strict v1
execution receipt, within this canonical service specification.

| Method | Path under `/control/v1` | Contract |
| --- | --- | --- |
| GET | `/tenancy/usage-evidence/capabilities` | [Capabilities schema](../../../crates/bitrouter-ai/src/tenancy/usage_evidence/capabilities.schema.json) |
| GET | `/tenants/{tenant_id}/executions/{execution_id}/usage-evidence` | [Execution evidence schema](../../../crates/bitrouter-ai/src/tenancy/usage_evidence/execution.schema.json) |

The packaged schemas and [DTO validator](../../../crates/bitrouter-ai/src/tenancy/usage_evidence.rs)
define the field catalog and cross-field constraints. Discovery requires
authentication and `Bitrouter-Installation-Id`. Clients MUST check the advertised
bounds, estimator and child-linkage facilities; a DTO or version number alone
does not imply availability. Generic server-tool capture is not supported in
version 1 and MUST NOT be advertised. Child linkage depends on the enabled build.

Reads require current receipt-read authority for the tenant and execution and
return `Cache-Control: no-store`. Installation, tenant and execution identities
MUST match the addressed resource. Correlation IDs are neither execution
identities nor inference idempotency keys. A client MUST NOT merge evidence by
correlation ID or replay inference to recover a missing observation.

#### Capture, absence and recovery

Evidence MUST be bound to its immutable terminal receipt and committed under
the same durable publication gate. A required document that is missing, corrupt
or bound to another receipt MUST fail closed. It MUST NOT be presented as a
historical receipt without capture.

| Capture state | Required interpretation |
| --- | --- |
| `captured` | Capture was installed; observations may still be empty, incomplete or truncated. It does not assert successful execution. |
| `not_captured` | Historical receipt predating this capture contract; no original observations are asserted. |
| `unavailable` | A version-1 admission seed proves capture was installed, but recovery lost the observations; retain an unknown outcome and durable unavailable evidence. |

Both absence states require empty attempts, frames and child IDs, null parent
and estimate, `estimation: not_captured`, `truncated: false` and
`dropped_frames: 0`. Historical seeds retain their original serialization and
absence semantics; unsupported capture versions fail validation. A new execution
MUST NOT be downgraded to historical `not_captured` after observation loss.
HTTP errors or an unsupported evidence endpoint do not assert either capture
state for a particular execution.

#### Native observations and ordering

Evidence attempts MUST preserve receipt dispatch order, exact attempt IDs,
targets and outcomes, together with whether a terminal was observed. Targets
may be null as allowed by the schema; populated targets identify provider,
model, account and credential revisions without exposing credentials. Some
observed usage MUST NOT turn an unknown execution outcome into a completed one.
Frames preserve repeated observations in arrival order with zero-based
contiguous sequence numbers and references to actual attempts.

Values MUST come from original upstream payloads before protocol conversion or
zero-filled canonical projections. Arbitrary upstream keys, strings, response
IDs, prompts, generated content and tool arguments/results MUST NOT be copied.
Only the closed quantity dimensions are retained, in their native token,
cache, reasoning, reported search or media units. A missing source quantity has
no entry; explicit zero remains zero; an invalid known value is null and sets
`invalid`. Unknown source fields set `unsupported` without retaining names or
values. Exact nonnegative integers are bounded by 9,007,199,254,740,991; consumers
MUST NOT round or synthesize quantities from content or default zeroes.

| Original protocol | Eligible source | Extractor version 1 semantics |
| --- | --- | --- |
| Chat Completions | JSON, HTTP error usage, accepted usage chunk | Snapshot |
| Responses | JSON, HTTP error usage, accepted terminal usage | Snapshot |
| Messages | JSON, HTTP error usage, message start, message delta | Snapshot |
| Decisions | JSON, HTTP error usage | Unknown |

Snapshots replace the observed dimensions; repeated frames MUST NOT be summed.
A Messages `message_delta` is cumulative despite its event name. Version 1
qualifies no additive delta source, even though the semantics enum names delta.
Unknown semantics MUST NOT be assigned an arithmetic interpretation. Malformed
or mislabeled streams cannot qualify a source merely by containing usage-like
fields.

Upstream usage field meanings remain defined by the
[Chat API reference](https://developers.openai.com/api/reference/resources/chat)
and [Messages streaming specification](https://platform.claude.com/docs/en/build-with-claude/streaming).
The extractor's qualified sources and bounded projection above do not extend
those upstream contracts.

#### Bounds, estimates and children

The original encoded response, including whitespace, is limited to 256 KiB.
The contract permits at most eight attempts, 512 frames per execution, 128 per
attempt, 32 quantities per frame and 64 child IDs. Capture retains an ordered
prefix. Once a frame cannot fit, truncation is sticky; `dropped_frames` counts
discarded observations with saturation at the exact integer maximum, and
`truncated` MUST equal `dropped_frames > 0`. Neither truncation, unsupported or
invalid data nor missing completion evidence establishes complete provider
totals or zero cost for unseen work.

`estimation` distinguishes unrecorded estimator provenance (`not_captured`), an
installed finalizer that did not use an estimate (`not_used`), and an estimate
actually used (`captured`). Only the last permits a nonnull `estimate`.
Algorithm `stream_character_ceiling`, version 1, rounds each prompt/output
Unicode scalar count divided by four upward. It retains the numerical inputs,
text/reasoning coverage and completed/disconnected trigger defined in the
schema. It excludes media, unseen output and tool execution. An estimate MUST
NOT replace native observations or be treated as a per-attempt provider amount
or a total over fallback and children.

Parent/child IDs MUST come from independently owned execution admissions,
never caller correlation or provider IDs. Parent and child retain separate
receipts and evidence; child quantities MUST NOT be folded into parent frames.
The schema rejects self-links, duplicates and a parent also listed as a child.
Consumers requiring linkage MUST check discovery before relying on it.

### Resource and evidence migration and rollback

Upgrading adds managed-head/revocation metadata and receipt-bound evidence;
it does not rewrite immutable resource revisions or historical receipt bytes.
Manifest-only resources are not silently adopted as remotely managed heads.
Historical receipts remain readable without backfilled or fabricated usage.
New evidence and resource messages keep their own wire discriminator 1 without
widening strict tenancy discovery or receipts. An old client can therefore use
the original API, but an old server is not thereby qualified to read a new store.

The resource lifecycle and usage evidence migrations refuse to adopt existing
objects at their new table names. Their down migrations refuse to drop existing
tables and require verified-backup rollback. No in-place downgrade of a store
that contains new heads, revocations, capture seeds or evidence bindings is
qualified. Before upgrade, preserve a consistent database backup and its
installation identity, wrapping key and compatible configuration. Rollback
requires stopping the sole owning process and restoring a verified compatible
set; simply deleting new tables/fields or starting an older binary MUST NOT be
used to bypass authority or evidence integrity checks. A restored snapshot
does not undo provider effects or prove the outcome of operations after it;
retain that uncertainty and reconcile it before admitting replacement work.
The [configuration migration rules](../configuration/README.md#tenancy-and-routing-compatibility)
also apply when changing binaries or startup settings.

### Resource provisioning compatibility cases

Conformance requires observable behavior from the actual producer and consumer,
not schema validation alone.

1. Reject missing required nullable CAS/policy fields, duplicate or unknown
   envelope fields, unsupported formats/enums, mismatched kinds, inexact
   integers, oversized commands/secrets and duplicate bindings/dependencies.
   Test compiler rejection separately from envelope schema rejection.
2. Concurrent creates or rotations have one CAS winner. Exact replay returns
   the original revision; a changed command under that operation ID conflicts.
   A lost reply is resolved with the original ID and identity-bound response.
3. Reads, operation records, errors and diagnostics contain no secret plaintext;
   wrong-key or wrong-reference retained ciphertext fails authentication.
4. Same-name resources remain owner-isolated. Tenant credentials cannot mutate
   pools or compile foreign private dependencies, including indirect closures.
   Reduced permissions take effect on subsequent checks.
5. Reject unapproved tenant origins and authority-overriding headers; accept
   valid approved endpoints. Removing an origin does not revoke a retained
   revision; explicit revocation fences its subsequent use.
6. Rotation preserves old immutable bytes and configurations. Revoking either
   an old or current revision fences cached dispatch and compilation/activation.
   Restart preserves the managed head and revocations. Creating a head beside
   manifest-only revisions preserves the old revisions.
7. Cancellation, shutdown and transaction faults leave one atomic
   mutation/outcome or an explicitly unknown result. Original-operation replay
   cannot create another revision; lookup remains subject to current authority.
8. Bounded HTTP/1.1 and HTTP/2 clients reject oversized or mismatched
   installation/operation/resource responses. Real service processes accept
   SDK-created resources and execute authorized JSON/SSE without manifest edits
   or recompilation.
9. An old strict discovery/receipt client still decodes the new server's original
   messages. A new client handles an absent provisioning endpoint or
   `provisioning_enabled: false` without attempting unsupported mutations.
   Invalid discovery never becomes a capability grant.
10. Migration retains old revisions, rejects unexpected preexisting lifecycle
    objects and refuses destructive downgrade. Verified-backup rollback
    preserves installation/key identity without claiming distributed safety.

### Usage evidence compatibility cases

1. Strict decoding rejects missing/duplicate/unknown fields, unsupported
   versions/enums, invalid integers, source/protocol/semantics mismatches,
   foreign attempt IDs, duplicate dimensions and oversized original bodies,
   including whitespace and chunked transport.
2. Repeated native frames preserve order and snapshot semantics. Zero, missing,
   invalid, unsupported and estimated observations remain distinct; arbitrary
   field names and strings never leak into evidence.
3. JSON/SSE preserve reported cache/search/media dimensions before conversion.
   HTTP failures and authorized credential-refresh attempts preserve separate
   attempt attribution. Malformed or mislabeled SSE cannot forge eligible usage.
4. Frame and byte exhaustion retain an ordered prefix and sticky truncation;
   later frames cannot reset completeness or silently disappear from the drop
   count. Saturation preserves the truncation flag.
5. Completed, rejected, partial and disconnected executions retain actual
   observations without claiming unseen work was free or successful. A missing
   read, lost response or broken stream never causes inference replay.
6. Estimates appear only when used, with the exact algorithm revision, numerical
   inputs and coverage, and never overwrite provider observations.
7. Receipt and evidence publication is atomic under storage faults, duplicate
   terminal delivery, shutdown and restart. Missing, orphaned, tampered,
   oversized or mismatched evidence fails integrity checks. Historical absence
   cannot mask deletion of a required document.
8. Recovery with a version-1 capture seed but no observations persists
   `unavailable` with unknown outcome; a historical seed produces
   `not_captured`. Preserve historical seed bytes and reject unsupported capture
   versions. Absence states cannot assert attempts, quantities or estimates.
9. HTTP/1.1 and HTTP/2 clients enforce the same bounds and exact
   installation/tenant/execution identities. Reads recheck receipt authority
   and return no-store responses; discovery alone grants no receipt access.
10. Advertised child linkage derives from independent admissions; parent and
    child ledgers remain separate without double counting. Generic server-tool
    capture stays unavailable.
11. Old strict receipt/discovery clients keep working. A new client treats an
    old server's absent evidence endpoint as unavailable functionality, never
    as `not_captured` for a named execution. Required evidence cannot silently
    fall back to normalized receipt usage.
12. Upgrade preserves historical receipts and adds evidence without rewriting
    them; unexpected existing evidence objects and destructive down migrations
    fail. Rollback uses a verified compatible backup, not deletion of bindings.

### Tenancy API compatibility cases

1. Discover the installation over an authenticated connection. A missing or
   mismatched installation binding on a protected read fails before disclosure.
2. Reject malformed, duplicate-header, duplicate-key and schema-invalid input;
   separately reject a schema-valid command that fails semantic validation.
3. A tenant credential cannot access another tenant; narrowing or revoking its
   current binding removes authority even if a prior request succeeded.
4. A resource-scoped grant cannot activate a whole-tenant configuration.
5. An exact authorized replay returns the existing result; a changed command
   with the same operation identity conflicts. A replay of credential issuance
   never returns its secret.
6. A receipt page with no matches succeeds with an empty page. Correlation
   reuse does not deduplicate an execution.
7. A timed-out write is reported as uncertain until independently resolved;
   an unauthorized lookup does not reveal another principal's operation.
8. When Eval is enabled, duplicate result replay reports duplicate, conflicting
   immutable content fails and all responses carry `Cache-Control: no-store`.
9. Strict dispatch rejects each missing, duplicated or malformed binding header,
   unsupported version/mode and caller tenant/principal overrides. An ineligible
   discovery descriptor cannot create an SDK binding. Restart/stale capabilities
   fail rather than silently selecting another instance or receipt mode.
10. A queued dispatch keeps its exact execution revision across activation;
    expired or narrowed credentials still fail current checks. Repeated
    correlation IDs produce distinct executions, with execution identity in
    the tenant response header.
11. Scoped SDK calls isolate per-call credentials/bindings over a shared pool.
    Wrong/missing/duplicate execution metadata, mismatched error identities or
    status codes, oversized bodies and premature SSE EOF cannot establish safe
    replay or success. Dropped streams and read timeouts send no replacement
    inference or remote-cancel request.
12. Successful terminal delivery follows durable receipt publication. Lost
    delivery is recovered by reading the original execution, without treating
    a missing receipt or provisional stream frame as a terminal outcome.

## Shared operator API

The operator listener is explicitly configured and authenticates a separate
bearer credential before parsing queries or bodies. Inference and legacy
control credentials do not authorize it. Its grants are `read`, `policy_write`,
`credential_write`, `publish`, `rotate_correlation`, `resolve_execution` and
`global_principal_write`. A write grant does not imply unrelated authority.
Actor identity is configuration-owned; requests do not select their own actor
or namespace. The operator listener binds to loopback; remote access requires an
authenticated protected tunnel or proxy. All responses have
`Cache-Control: no-store`.

| Method/path | Query or body |
| --- | --- |
| POST `/operator/v1/mutations` | JSON `Mutation` with `operation_id` and `command` |
| GET `/operator/v1/operations` | `operation_id` |
| GET `/operator/v1/revision` | No query parameters |
| GET `/operator/v1/tenants`, `/resources`, `/principals`, `/executions` | `id` |
| GET `/operator/v1/credentials` | `provider`, `account` |

For example, a synthetic resource mutation is:

```json
{
  "operation_id": "create-pool-1",
  "command": {
    "action": "put_resource",
    "input": {
      "resource_pool_id": "provider-account-1",
      "expected_revision": null,
      "concurrent_attempts": 8
    }
  }
}
```

The tagged command has `action` and `input`; supported actions are
`put_tenant`, `put_resource`, `put_principal`, `import_credential`, `publish`,
`rotate_correlation` and `resolve_execution`. Fields, revisions and result
shapes are defined by the [public DTOs](../../../crates/bitrouter-ai/src/gateway/operator.rs).
The request body is limited to 2 MiB, requires JSON, and rejects content
encoding and unknown fields. Reads return metadata, never credential plaintext.
Publication uses an expected configuration generation. Principal updates
require their separately granted database-wide authority.

A mutation's caller-chosen `operation_id` is bound to actor and command. An
identical replay under the same actor returns the stored receipt; a conflicting
reuse is rejected. A timeout on a write is `unknown`, not proof of rollback.
The client SHOULD query the same operation as the same actor, and MUST NOT
resubmit it with a different ID merely because the response was lost. A receipt
can remain `unresolved`; successful lookup is not proof of failure or rollback.
Mutations do not themselves dispatch inference.

Errors are JSON `{ "code": "...", "operation_id": ... }`, with nullable
operation ID: 400 `invalid_input`, 401 `unauthorized`, 403 `forbidden`, 404
`not_found`, 409 `conflict`, and 503 `unavailable` or `unknown`. A client MUST
handle unfamiliar error codes as failures. Query and mutation shapes reject
unknown fields; do not assume future commands or enum values can be executed.
See the [operator guide](../../../skills/bitrouter/references/shared-gateway.md#operator-listener).

### Shared Operator API compatibility cases

1. An inference or legacy-control token cannot authenticate the operator
   listener. Insufficient grants fail before a mutation is applied.
2. Unknown query/body fields, non-JSON or encoded mutation bodies and oversized
   input are rejected. Responses include `Cache-Control: no-store`.
3. An exact actor/operation replay returns the original receipt without applying
   the command twice; changed input with the same identity conflicts.
4. A timed-out mutation remains unknown until a same-actor receipt resolves it;
   an unresolved receipt is not reported as rollback or successful execution.
5. Credential read projections do not disclose imported credential plaintext.

## Eval exchange API

The optional `core` feature exposes the evaluator-neutral evidence exchange.
These endpoints append and read evidence; they do not publish a policy lock or
start a model invocation. Authentication follows the configured gateway
credential policy, including explicit development `skip_auth` configuration;
production clients MUST use the host's configured authentication boundary.

| Method | Path | Body / response |
| --- | --- | --- |
| POST, GET | `/v1/evals/subjects` | `EvalSubject` / insertion report or subject list |
| GET | `/v1/evals/subjects/{eval_id}` | Subject |
| POST | `/v1/evals/results` | `EvaluationResult` / admission outcome |
| POST | `/v1/evals/snapshots` | Optional `FreezeRequest` / frozen snapshot |
| GET | `/v1/evals/snapshots/{evidence_root}` | Frozen snapshot |
| GET | `/v1/evals/status` | Ledger status |

Fields, identity, numeric units, result and error envelopes are defined by the
[public Eval types](../../../crates/bitrouter-ai/src/eval.rs). Results refer to
subjects; evidence snapshots represent frozen membership, not live policy.
Subject and result identity are immutable: matching replay is distinguished from
conflicting data. Observation of transport success or cost does not by itself
establish task quality. Clients MUST handle authentication, invalid input,
missing subjects and server failures as failures, and MUST NOT use a network
failure as proof that a prior write did not commit. Unknown optional response
fields may be ignored; unknown required semantics MUST NOT be guessed. See the [operator workflow](../../../skills/evaluating-bitrouter-routes/references/eval-exchange.md).

### Tenant Eval profile

This additive control-listener contract exposes the existing evaluation ledger
DTOs in an authenticated tenant namespace when the `core` feature is enabled.
It does not change the management mutation bodies, sixteen management operations,
legacy `/v1/evals/*` endpoints, or public EvalStore methods.

#### Authentication and installation binding

Every operation requires a tenant bearer credential and exactly one
`Bitrouter-Installation-Id` header, including POST. Existing Eval request DTOs
remain unchanged; the installation header is the binding for this new transport.
A body field, query parameter, public caller context, host inspection credential,
installation operator, audit credential, or legacy virtual key cannot replace it.

The credential and its current recipient binding must both cover the whole
path tenant. Current credential state, expiry, binding, tenant state and permission
intersection are checked again after storage waits and before commit. Resource-only
grants cannot use this tenant-wide ledger. Operator pool grants are not required.

| Method | Path under `/control/v1/tenants/{tenant_id}/evals` | Request | HTTP 200 body | Permission |
| --- | --- | --- | --- | --- |
| POST | `/subjects` | `EvalSubject` | `PutSubjectReport` | `evaluation.submit` |
| GET | `/subjects` | None | `Vec<EvalSubject>` | `resource.read` |
| GET | `/subjects/{eval_id}` | None | `EvalSubject` | `resource.read` |
| POST | `/results` | `EvaluationResult` | `AdmissionOutcome` | `evaluation.submit` |
| POST | `/snapshots` | Empty body or `FreezeRequest` | `EvalSnapshot` | Both permissions |
| GET | `/snapshots/{evidence_root}` | None | `EvalSnapshot` | `resource.read` |
| GET | `/status` | None | `StatusReport` | `resource.read` |

No query parameters are accepted. POST subjects/results and nonempty snapshot
bodies require JSON. Duplicate JSON keys are rejected at every nesting level.
Malformed nonempty snapshot bodies and unknown FreezeRequest fields are errors;
only an empty body or an omitted/null `frozen_at` chooses the service clock.
All responses, including errors, carry `Cache-Control: no-store`. HEAD inherits
GET authorization. Status retains the existing `heldout` count key.

#### Initial evaluator profile

Manual results must identify the authenticated stable principal exactly as
`evaluator.authority_id`, use `EvaluatorKind::Generic`, and contain no
`hard_violations`. Other evaluator fields remain submitted provenance. Requests
are rejected rather than rewritten. Host EvalConfig cannot confer a trusted rank,
metric wildcard, or permission. Full result-for-subject validation still applies.
Manual imports work with learning off and do not initiate providers or learning
jobs, consume evaluation-job allowances, or activate policies.

#### Atomicity and retry semantics

A result, every resulting prior-result dispute, and its terminal admission event
are written in one transaction. Exact authorized replay returns the existing
latest outcome with `duplicate=true` and appends no events. Changed immutable
content is rejected. Missing admission closure is an integrity failure, not an
automatic repair. Rotated credentials for the same principal can replay without
changing creator provenance.

Owned snapshot freeze selects membership, computes the existing tenant root,
persists that exact manifest and validates its stored winner in one stable
transaction. `frozen_at` labels the manifest; it is not an as-of query or a client
idempotency key. A supplied `frozen_at` is validated as RFC3339 and its exact
decoded string is preserved in the manifest, reply and root calculation; there
is no timestamp normalization. An absent or null value chooses the actor clock
once for that operation. Historical snapshots retain their immutable closure
after later admission changes. Legacy root bytes and public freeze wrappers are unchanged.

The existing authority actor serializes ledger writes with management operations.
A revoke acknowledged before a job executes wins. A revoke queued during a ledger
transaction runs after that transaction. Expiry before commit dispatch causes
explicit rollback; expiry during COMMIT can have an unknown durable outcome.
Postcommit revocation can suppress a success response without removing evidence
that already committed. Bytes already handed to transport cannot be recalled.

#### Bounds and failures

The endpoint bounds headers to 128 values / 64 KiB, subject/result wire and
canonical JSON to 1 MiB, freeze bodies to 4 KiB, each collection/conflict set to 1,024 rows,
cumulative serialized SQL/manifest/write work to 32 MiB, and an encoded success
response to 8 MiB. Collection overflow fails instead of truncating. These are
payload/work bounds, not a claim about allocator overhead or process RSS.
Eight ingress leases cover body collection, queued work and response-body custody.
Work has a five-second absolute deadline including queue time. Precommit cleanup
gets one additional second from its first stop/error; missed cleanup fences new
authority while the actor retains the transaction. In-flight commit at the work
deadline fences immediately. No transaction is aborted or retried automatically.

Failures use the existing tenancy Error envelope with null operation/execution
IDs and `dispatch_state=not_started`. That dispatch state describes provider
work, not whether a ledger transaction committed. Invalid input/content is 400;
invalid credentials are 401; denied profile/grants or inactive tenants are 403;
wrong installation binding is 409; authorized absence is 404; oversized bodies
are 413. Confirmed ordinary capacity rejection is 429. Storage/integrity failure,
uncertain cleanup, or a response waiter timing out after enqueue is 503. A waiter
timeout is never evidence of rollback. No automatic retry is performed.

Already admitted execution observations use the same actor lane under existing
execution custody and their own bounded deadline. Their failure is retained and
makes shutdown unsuccessful even if database rollback succeeded. Cleanup does
not require a newly granted manual evaluation permission. This transport does
not enable production dispatch, strict receipts, or distributed writers.

### Eval Exchange API compatibility cases

1. An authenticated producer stores a subject and a result and reads only
   evidence available to its configured owner. A missing subject is not success.
2. An identical result replay is distinguished from changed immutable content.
3. A frozen snapshot remains bound to its original evidence membership after
   new results are submitted.
4. Posting evidence does not publish or mutate the active routing policy.
5. A lost response does not prove that a subject or result was not committed.

## Thread API

The optional host listener binds to loopback and uses a dedicated execution
bearer, not an inference or control token. `GET /agent/v2/capabilities`
authenticates and returns the server instance, runtime capabilities, operations
and version. Other operations carry `X-Bro-Server-Instance` identifying the
discovered instance. A client MUST reconnect and rediscover after an instance
change and MUST NOT silently replay a mutation against the new instance. A
workspace must be explicitly authorized by the host. Authentication or
possession of a Thread ID does not grant a different workspace.

| Method/path relative to `/agent/v2` | Operation |
| --- | --- |
| GET `/capabilities` | Discover available operations |
| POST `/threads` | Create thread |
| GET `/threads/{id}` | Read thread |
| POST `/threads/{id}/turns` | Start or enqueue a turn |
| GET `/threads/{id}/turns/{turn_id}` | Read turn |
| POST `/threads/{id}/turns/{turn_id}/cancel` | Cancel selected work |
| POST `/threads/{id}/steer`, `/threads/{id}/inputs`, `/threads/{id}/resume` | Steer, answer input, or resume queue |
| GET `/threads/{id}/history`, `/threads/{id}/observe` | Paginated committed history or SSE observations |

Mutation requests carry a caller-owned ASCII `Idempotency-Key` of 1–128 bytes.
A matching authorized replay and conflicting reuse are distinguished by the
server; a client MUST NOT generate a new key solely because a response was
lost. Mutation JSON is limited to 64 KiB. `mode` on a turn distinguishes
`start` and `enqueue`; a receipt is admission, not successful completion.
Disconnecting an observer does not cancel accepted work. History query defaults
`after` to 0 and `limit` to 128; the server remains authoritative on limits.

JSON responses are versioned where described by the [public API types](../../../crates/bitrouter-ai/src/harness/api.rs).
Errors use `{ "error": "...", "code": "..." }`; SSE can carry an `error`
event with `{ "code": "...", "message": "..." }`. A client MUST handle
unknown error codes as failures and resynchronize when required. The API's
request types are not uniformly closed to unknown fields; no blanket unknown
field rejection or forward-preservation guarantee is implied. Feature-disabled
operations MUST NOT be inferred from their path alone.

### Thread API compatibility cases

1. The dedicated bearer is required; capabilities identify the serving instance
   and do not advertise disabled native execution operations.
2. A request targeting an unauthorized workspace or a stale server instance is
   rejected. A client rediscovers after restart before a new mutation.
3. Matching mutation/key replay does not execute twice; changed content using
   the same key conflicts. Missing, non-ASCII or oversized keys fail.
4. Enqueued turn admission is not completion. Disconnecting observation does
   not cancel accepted execution; history remains accessible with authority.
5. An SSE error and a truncated stream do not become a successful terminal.

## Managed Core API

An active virtual service key authenticates `GET /v1/orchestrator/capabilities`
even if ordinary inference is configured to skip authentication. Availability
depends on the host's `core` feature and enabled service. The response identifies
the core instance, supported operations/transports and limits. A client MUST
honor the advertised capabilities rather than assume the endpoint's presence
implies every operation is enabled.

`GET /v1/orchestrator/channel` upgrades to WebSocket only with
`Bitrouter-Beta: orchestrator_core=v1`. Managed `POST /v1/responses` uses the
same beta header; the ordinary Responses profile remains separate. The caller
must first bind or restore a session over the core channel within the host's
initial-message deadline. The harness owns durable checkpoint persistence and
must explicitly acknowledge the checkpoint only after persistence; HTTP/SSE
observation does not acknowledge it. DTOs and message variants are defined by
[core protocol](../../../crates/bitrouter-ai/src/core/protocol.rs),
[checkpoint](../../../crates/bitrouter-ai/src/core/checkpoint.rs) and
[managed Responses](../../../crates/bitrouter-ai/src/core/responses.rs).

A channel disconnect or dropped HTTP stream does not itself complete or cancel
a run. No automatic replay or exactly-once result after uncertain delivery is
promised. A client MUST use the existing operation inspection/recovery semantics
before deciding on a new operation. `CoreError` carries `code`, `message` and
`commit_status` (`not_committed`, `committed`, `unknown`); `unknown` MUST NOT be
interpreted as rollback. Strict input structures reject unknown fields where
specified by the DTO. Unknown required capabilities or future message variants
MUST NOT be guessed.

### Managed Core API compatibility cases

1. Require a valid service key for capabilities even when ordinary inference
   skips auth. Reject an unsupported beta value before channel upgrade.
2. Require bind/restore within the initial deadline; unsupported operations
   fail rather than being inferred from a successful handshake.
3. Persist a checkpoint before sending its explicit ACK; observing HTTP/SSE
   output alone does not acknowledge a checkpoint.
4. Dropping the channel or observation does not establish cancellation or
   rollback. Treat `commit_status: unknown` as an unresolved outcome.
5. Reject malformed strict message input and unsupported required semantics.

## Native ACP extensions

### Connection and capability discovery

`bro acp serve` starts the native stdio bridge; `bro acp serve <agent>` is the
separate external-agent proxy. A native bridge connects to a local daemon; its
stdio carries ACP protocol messages. Native mode needs an explicit model or
`chat.model`. Initialization negotiates supported ACP v1 or v2 for the
connection; unsupported versions and unsupported methods fail visibly.
Consumers MUST use the advertised capability set instead of assuming that
optional methods exist. The native adapter's `_meta.bitrouter.version` is `1`;
its `schemaVersion` is `1.10.2`. `nativeExecution` reflects the `core` build
feature. Queue resume and close are advertised only when supported.

### Session and execution

An ACP session identifies a native thread. Supported lifecycle operations
include initialization, new, list, prompt, cancel, and version-appropriate
load/resume; close and queue resume depend on advertised native execution.
The v1 prompt returns its result when the turn settles. The v2 prompt returns
acceptance with the admitted user message identity and completion is projected
through updates. Consumers MUST NOT treat acceptance as successful completion.

A client disconnect detaches transport; it does not cancel accepted work.
Cancellation cancels active work and leaves accepted unstarted work in a
paused queue. Explicit close cancels active and queued work while retaining
history for authorized reopening. Pending approval is native state and may be
reattached on reconnect to the same live daemon; transport request IDs do not
confer approval authority. The advertised `_bitrouter/session/resume_queue`
extension resumes a paused queue subject to current authorization. Client info,
requested working directory and possession of a session ID do not grant access.

### Operation identity

Retry-capable clients may supply `_meta.bitrouter.idempotencyKey` for durable
operations. The key is scoped to the caller, Thread and operation; JSON-RPC
request IDs are transport identities, not idempotency keys. A matching retry
returns the original receipt and changed content conflicts. A client that lost
an acknowledgement without a reusable key SHOULD inspect authoritative history
rather than automatically submit a new prompt. Load or resume reattaches; it
does not itself resume a paused queue. A busy or queued prompt is rejected
rather than silently reordered.

### Validation and evolution

Invalid members in `mcpServers`, `prompt` or `additionalDirectories` are rejected
rather than dropped. Non-null `_meta` must be an object. A peer MUST NOT assume
that enabling draft v2 enables every draft extension, or that a response from a
future schema revision has the same meaning. Unknown optional metadata MAY be
ignored; unsupported required capabilities MUST fail visibly. See the [native adapter](../../../apps/bitrouter/src/harness/acp/native).

### Native ACP extensions compatibility cases

1. Initialize v1 and v2 separately and verify version-specific capability
   advertisement. Unsupported versions and methods fail rather than downgrade.
2. For each enabled version, create a session and prompt. In v2, acceptance is
   not reported as completion before the terminal update.
3. Disconnect after admission; reconnect with current authority and observe the
   same session without canceling the accepted turn.
4. Cancel active work and verify accepted queued work stays paused. Close an
   enabled session and verify active and queued work are canceled but history
   remains available to an authorized client.
5. Reject an invalid array member instead of silently dropping it. A session ID
   or client-provided working directory does not bypass authorization.
6. With core disabled, do not advertise native execution/queue controls that
   depend on core. Approval reattachment must not create a second approval.
7. Retry the same durable operation with its namespaced key and verify the
   original receipt; a different payload with the same key conflicts.

## Shared request identity

This section applies to the shared gateway profile. The exclusively owned local
[tenant dispatch profile](#strict-tenant-dispatch-binding) has its own execution
response identity and does not acquire these idempotency semantics.

Client correlation IDs, durable execution IDs, provider-attempt IDs and public
continuation IDs remain distinct. Shared execution/attempt IDs are generated by
the service. Reusing `x-bitrouter-request-id` does not overwrite another request's
metering or imply idempotency.

`Idempotency-Key` is optional and bounded. The durable binding is scoped to the
cluster and authenticated tenant, and binds the operation, authenticated
principal identity and canonical request fingerprint. Fingerprint version 1 uses
the validated ingress JSON before configurable prompt transforms, preset/default
expansion or route selection, with object keys sorted recursively. It includes
the endpoint and an explicit allowlist of execution-affecting headers. It
excludes mutable auth/config revisions, transient trace/correlation IDs and raw
authorization credentials. Admission revisions are stored separately. Duplicate
lookup requires fresh current authorization and the same principal identity;
it never recomputes the prior execution under a newer routing configuration.
Key reuse with a
different fingerprint or operation returns 409 before dispatch. A duplicate
returns 202 with the same durable execution status and a query location, rather
than replaying inference or pretending to resume a stream. Ordinary first
submissions retain their protocol response/SSE behavior.

Version 1 uses the following fixed lowercase header names, in this order:

```text
anthropic-beta
anthropic-workspace-id
chatgpt-account-id
openai-organization
openai-project
session-id
thread-id
user-agent
x-bitrouter-acp-session-id
x-bitrouter-benchmark-run-id
x-bitrouter-controller-id
x-bitrouter-harness
x-bitrouter-parent-session-id
x-bitrouter-trial-id
x-bitrouter-workflow-session
x-claude-code-agent-id
x-claude-code-parent-agent-id
x-claude-code-session-id
x-codex-turn-metadata
x-goog-user-project
x-session-id
x-smithers-node-id
x-smithers-workflow-id
```

The SHA-256 input is the compact UTF-8 JSON array
`["gateway-request-v1", endpoint, [[name, [raw_value_bytes, ...]], ...], body]`.
Each header value is an array of byte integers. Duplicate value order and
multiplicity are retained, including empty values; an absent header has no
values. JSON object keys are sorted recursively, while array order and scalar
values are preserved. No body serialization is retained alongside the parsed
body. This encoding and header set cannot change silently across replicas.
Publication rejects resolved provider passthrough rules outside this set;
static provider headers remain supported. Shared session context does not use
process-local ACP route leases.

A compatibility vector uses `/v1/responses`, `openai-project` values `p1`, `p2`
in that order, one empty `session-id`, all other listed headers absent, and body
`{"seed":7,"model":"fixture","input":["雪",{"z":null,"a":false}]}`.
Its digest is
`b8140196b302d60f648816e372820e1c133e96d73d6158df89b172b557972777`.

Shared Responses accepts ordinary inference only. Any `BitRouter-Beta` header,
any top-level `bitrouter` field, or `multi_agent.enabled: true` is rejected with
`unsupported_capability` before admission or duplicate lookup. Capacity is
reserved before inspecting either headers or the bounded JSON body.
`multi_agent.enabled: false` alone does not select the managed profile.

`GET /v1/gateway/executions/{id}` returns a bounded, tenant-authorized status
projection. Every response after acceptance, including an error response, exposes
`x-bitrouter-execution-id` and the query location in addition to the existing
correlation header. Recovering after loss of the entire response requires an
idempotency key, because the client may never have learned the generated ID.
An admission storage failure may leave the commit outcome unconfirmed. That 503
also exposes the candidate execution ID and status location; these headers
permit investigation and do not assert that acceptance committed. A query can
return 404 if no execution committed. The caller's correlation ID remains
separate from the execution ID used by pipeline hooks, metering and public
continuation handles. Duplicate responses echo the current request correlation
in their header while retaining the original correlation in the execution DTO.
The SDK explicitly distinguishes
an existing execution from a model response. Unknown executions are queryable;
the service does not promise provider-side exactly-once execution.

The SDK's additive `gateway::execution` surface provides `SubmissionOptions`,
`ExecutionMetadata`, `Submission<T>` and `GatewayClient::execution(id)`. The four
`submit_*` JSON methods and three `submit_stream_*` methods require shared
metadata and distinguish 200 output from 202 status before protocol decoding.
Original model methods remain local-compatible and reject 202 with its HTTP
metadata retained. No new SDK feature or service dependency is required.

Options accept only the fixed semantic header set above, preserving duplicate
values; authentication and managed-profile overrides are rejected locally.
Generation methods reject an extension-map `stream` field before sending, so it
cannot override the typed stream mode during serialization. Original correlation
in a 202 execution DTO is independently validated as normalized 1–256 ASCII bytes;
it need not equal the current response correlation. Successful response
Content-Type must be one complete media type with valid parameter syntax.
New submission methods use a stable default `bitrouter-sdk` User-Agent so an SDK
version string alone does not change the request fingerprint. Explicit caller
values remain semantic. This does not promise serialization stability across
arbitrary SDK upgrades.

Status URLs are constructed from the client's configured origin and proxy prefix
with one encoded execution-ID segment. Validated Location is evidence, never a
credential-routing authority. Shared response metadata survives malformed,
oversized or truncated 200/202 responses in the existing `Error::Service`
container with the actual wire status and a `body_error`. Calling a shared-only
submission method against a local server may execute inference before missing
metadata is detected; the SDK never probes, retries or falls back automatically.
See the [SDK usage and recovery contract](../../../crates/bitrouter-sdk/README.md).
