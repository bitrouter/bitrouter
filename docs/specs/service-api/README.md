# BitRouter service API

Status: **accepted, `service-api/1.0.0-beta.6`**. The [code constant](../../../crates/bitrouter-ai/src/control/mod.rs)
identifies this contract independently of transport versions. This is the single
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

Changes require review of compatibility in both client/server directions.
Do not silently change a field's type, units, default, nullability, enum meaning,
identifier scope, error semantics, terminal event or fingerprint encoding.
Capability discovery is required where offered; an unknown optional response
field can be ignored only where that message's schema permits extensions.
Unsupported required behavior must fail visibly.
Request strictness varies by schema; adding a request field is not automatically
compatible with a reader that rejects unknown fields. Schema/DTO changes,
examples and compatibility cases must be reviewed together. Feature availability
and authorization remain distinct from format compatibility.

## Control API

### Scope and discovery

The optional authenticated control listener serves operator inspection and
reload. It is distinct from the inference gateway and its credentials. A client
MUST authenticate before using `GET /control/v1/capabilities`; it MUST use the
reported resources, actions, scopes and schemas to determine what is available
for its credential and host. A missing action is not an authorization grant.
The capability response includes `protocol`, `protocol_version`,
`server_version`, `action_descriptors`, `resources`, `limits`,
`scopes` and optionally server and artifact identities. Descriptors contain
per-action input/output JSON schemas. An unknown action MUST NOT be invoked
merely because its identifier is recognizable.

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

### Control descriptors

Capabilities MUST identify `protocol: bitrouter-control` and
`protocol_version: 2`. Before invoking any action the SDK checks its
descriptor ID, version, required scope and schemas through discovery.
This applies to direct SDK calls and CLI client calls.

| Descriptor ID | Version | Required scope |
| --- | --- | --- |
| `status` | 1 | `control:read` |
| `list_models` | 2 | `control:read` |
| `route` | 2 | `control:read` |
| `requests` | 1 | `control:read` |
| `providers_list` | 1 | `control:read` |
| `observe_status` | 1 | `control:read` |
| `policy_status` | 2 | `control:read` |
| `policy_show` | 2 | `control:read` |
| `agents_list` | 1 | `control:read` |
| `reload` | 2 | `control:reload` |

`PolicyReport` includes `view`, `availability`, nullable `source_digest`,
`policies`, router-to-literal-name `bindings` and nullable typed
`source: policy::Document`. Show validates a supplied selected name and
returns the complete resubmittable document. The digest covers that whole
source. Active is the default view and disk must be requested explicitly.
Unavailable source fails without substitution. The associated tenant API
reads `status`, `document` and the literal `name` for an exact resource
reference.

An incompatible descriptor fails before invocation. Direct malformed requests
return 400 `invalid_request`. Unsupported actions return 404
`unsupported_action`. The server checks scopes and validates the selected
configuration and Policy source before changing live state.

### Authentication and isolation

Credentials are bearer tokens in `Authorization`. `control:read` grants
inspection; `control:reload` authorizes reload and requires `control:read`;
`control:tenancy` is separate and does not imply either. Host control tokens
are independent of inference credentials. The default control token grants read-only access. The listener's deployment and TLS requirements are operator settings;
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
   request and response schemas. A client rejects a missing or incompatible required descriptor.
3. Submit a valid reload using the sampled instance/generation. Disconnect and
   look up the same request ID on the same boot; the operation continues.
4. Replay the identical body under the same credential and verify no second
   reload is admitted. Reuse the ID with changed input: 409 conflict.
5. Stale generation, another boot and concurrent reload do not start a second
   operation. Unknown/expired results are not represented as a confirmed rollback.
6. Every direct and CLI action checks its descriptor. Reject unsupported
   protocol versions, missing descriptors and wrong versions, scopes or schemas.
7. A partially applied operation is a terminal report with participant outcomes,
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
envelope and cursor. The byte bound or a smaller bounded SQL working page can
produce a shorter page. The server may halve a working page only after a typed
capacity refusal, in the same read snapshot and original deadline, down to one
returned entry plus its continuation sentinel. This never retries a mutation,
skips a record, or increases the caller's requested limit or working allowance.
Empty results
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

The SDK method is
`TenancyClient::directory(installation, authority_revision, after, limit)`.
It validates installation, requested revision, ordering, continuation and count,
and caps received bytes at 1 MiB or the client's lower configured response bound.
The DTO rejects unknown fields and variants; clients must fail visibly on
unsupported schema versions.

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

The entire compact UTF-8 JSON encoding of either `GrantScope` variant, including
its `kind` tag, is bounded at 262144 bytes (256 KiB). There is no separate
256-resource cardinality ceiling. The validator counts encoded bytes into a
bounded sink before allocating its uniqueness set; this does not claim that an
HTTP decoder has not already allocated the input. Resource identities, duplicate
rejection, nonempty resource scopes and operator-pool ownership rules are
defined by each message. JSON Schema describes the shape and uniqueness; the typed validator
enforces the serialized-byte bound. Runtime working and retained-view budgets
remain separate, lazy, operator-configured capacity constraints. Sequential
point reads charge actual retained record lengths before loading their links;
startup can reduce its directory working page on capacity refusal without
skipping any records. Operation and retained-record validation run once with
their original budgets; their preflight refusals do not trigger directory
retries.

Management request bodies are bounded to 2 MiB and directory responses to
1 MiB. Clients MUST preserve scope exactly. A resource allowlist cannot be
truncated or converted to tenant-wide authority.

The configuration and its referenced route/policy revisions are pinned for
execution. Current health, credential and authority changes can narrow eligible
targets, not expand an admitted envelope. Suspended or deleting tenants cannot
start work; already admitted work can still finalize under its existing
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
expiry the operation ID remains permanently reserved. The current local store
retains its indexed operation record and does not impose a lifetime operation
count quota or recycle IDs. Active work, queues, reads and storage admission
remain bounded by the startup host budgets. A principal can recover its own
unexpired operation only while current authority covers its saved scope.

Host budgets are defined by the [configuration contract](../configuration/README.md#process-capacity).
The store preserves indexed operation IDs, cumulative counters and stop state.
Startup validates the store before acquiring authority.

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

The [execution receipt](../../../crates/bitrouter-ai/src/tenancy/execution.rs)
identifies an installation/execution, its dispatch mode, actual attempts,
configuration and available usage evidence. Ordered receipts include the
accepted selection and original ordinals. Unknown quantities remain null;
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

Clients MUST discover required resource and evidence capabilities through
their respective endpoints. An absent endpoint or disabled capability makes
the facility unavailable. Authentication, transport or response-validation
failure does not establish capability or evidence absence.

### Resource provisioning

`Definition::Policy.document` and shared publication's `model_policy` use
the typed [Policy Document](../policy/README.md). The resource revision,
HTTP namespace and source contract are independent. Unknown or duplicate
fields and unsupported source fail before mutation.

`GET /control/v1/tenants/{tenant_id}/resources/policy` takes the same exact
`ResourceRef` query and installation binding as resource-status reads. It
requires live `resource.read` authority, loads authenticated immutable content,
then revalidates authority before disclosure. Its response is
[`PolicySource`](../../../crates/bitrouter-ai/src/tenancy/resources.rs):
`status`, `document` and selected `name`. It returns only representable model
Policy source. Other resource kinds are invalid requests; experimental artifacts
return `capability_mismatch`. Generic resource-status reads remain nonsecret
status only.

`GET /control/v1/tenancy/policy-capabilities` uses the same installation binding
and live capability authority as resource discovery. `PolicyCapabilities`
returns `installation_id`, exact supported `spec_version`, installed
`classifier_digest`, and `provisioning_enabled`. It neither authorizes a
resource nor promises that a later mutation will succeed.

Policy compatibility is checked over the complete reachable dependency closure
when a managed route is submitted, a configuration is created, an active
configuration is restored, and an execution view is opened. Referencing an
operator-imported experimental policy does not bypass Policy admission.
Tenant/shared import, configuration and execution reject experimental routes
(core_choice, assessment, block and hidden dependencies), even in all-feature
builds and without Policy. Exact literal selected names have no variant fallback.

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
| GET | `/tenancy/resource-capabilities` | `ResourceCapabilities` |
| POST | `/tenants/{tenant_id}/resources` | `ResourcePut` / `OperationResult` |
| POST | `/tenants/{tenant_id}/resources/revoke` | `ResourceRevoke` / `OperationResult` |
| GET | `/tenants/{tenant_id}/resources` | Exact `ResourceRef` query / `ResourceStatus` |

The [resource DTOs and validators](../../../crates/bitrouter-ai/src/tenancy/resources.rs)
define the fields and semantic constraints. The put command accepts the closed
set of resource definitions, including credential slots, model-only Policies,
admission profiles and static routes with explicit profile attachments.
The canonical schemas are [put](../resource-provisioning/put.schema.json),
[capabilities](../resource-provisioning/capabilities.schema.json),
[revoke](../resource-provisioning/revoke.schema.json), and
[status](../resource-provisioning/status.schema.json). The current put format is
`1`; `profiled_route` attachments address Chat Completions, Messages, Responses,
and GenerateContent explicitly. Admission profiles require `health_group_id`
exactly when health admission is enabled. Group identity is scoped to the
installation and operator pool; member RPM remains independently rotatable.
Ordered callers select `unprofiled` or `profiled` admission explicitly.

Discovery requires `Bitrouter-Installation-Id`. It reports which definitions
can be provisioned and their bounds. Provisioning is disabled without a
configured wrapping key. Upstream secrets are distinct from API bearer
credentials.

#### Creation, rotation and authority

The required `expected_revision` is null only for creation of a remotely managed
head; otherwise it MUST equal that head. The service assigns a new immutable
revision. Rotation MUST NOT rewrite old definitions or retarget configurations
that selected them. Creating a managed head for a manifest-only key preserves
its existing revisions. Resource kinds are `route`, `policy`, `credential`,
`configuration` and `execution`.

Resource identity includes owner kind, owner ID, resource kind and resource ID;
an exact reference additionally includes revision ID. Authority revision and
content revision are distinct and MUST NOT be substituted for each other.

An admission profile is an operator-pool Policy resource with explicit
nullable rate and health rules, a required nullable `health_group_id` and a
fallback rule. Health and group MUST both be null or both be non-null.
Zero RPM denies admission locally. Null rate disables that gate. The profile
is bounded to 4 KiB. Multiple members may share one health circuit.

An attached route binds every compiled protocol root and original candidate
ordinal to its exact credential and profile revision. Compilation verifies
total, unique attachment coverage. A profile or attached route requires
installation authority, even when the route has a tenant owner. A profiled
key remains protected across all retained revisions. Missing or revoked
dependencies fail authority checks. Rotation preserves definitions and
readability of previously admitted evidence.

#### Shared health profiles

Group identity is scoped to the installation and operator-pool owner.
All retained members, including revoked and unreferenced members, must have
identical health rules. Member revisions, RPM, credential and route
identities do not partition group health. Members may have distinct rate
groups, ceilings, read-only rate associations and fallback rules. Only a
member's RPM can rotate. Changing health rules requires a new group and
member keys, followed by explicit route and configuration publication.
Concurrent group creation uses the serialized authority writer.

A shared circuit grants no resource or dispatch authority. One half-open
probe owns the group. A stale probe result cannot complete a newer lease.
Rate-denied probes release neutrally and charged timestamps stay charged.
A profile with null health allocates no circuit state. Its optional
read-only rate association exposes the shared count without charging.
Observation cannot allocate state or claim a probe.

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
Revocation applies to any retained revision. Revocation MUST fence subsequent
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

### Ordered static dispatch

Ordered dispatch is an explicit mode on the existing `/v1/chat/completions`,
`/v1/messages` and `/v1/responses` endpoints. It uses the same protocol bodies,
JSON/SSE rendering and current-authority send gates. It does not add inference
idempotency, a retry API, a scheduler callback or a separate dispatcher.

| Method | Path under `/control/v1` | Purpose |
| --- | --- | --- |
| GET | `/tenancy/ordered/capabilities` | Authenticated dispatch discovery before installation binding |
| GET | `/tenancy/tenants/{tenant}/configurations/{revision}/candidates` | Operator-only descriptors for an exact retained protocol root |
| GET | `/tenants/{tenant_id}/executions/{execution_id}` | [Execution receipt](#receipts-and-pagination) |
| GET | `/tenants/{tenant_id}/executions/{execution_id}/usage-evidence` | [Original usage observations](#execution-usage-evidence) |

The [ordered dispatch schema](ordered-dispatch.schema.json) and the public
[selection](../../../crates/bitrouter-ai/src/tenancy/ordered.rs),
[receipt](../../../crates/bitrouter-ai/src/tenancy/execution.rs) and
[evidence](../../../crates/bitrouter-ai/src/tenancy/usage_evidence.rs) validators
define required fields, nullability, versions and semantic constraints. All
reads other than discovery require `Bitrouter-Installation-Id`; protected
responses use `Cache-Control: no-store`.

#### Discovery, selection and authority

Ordered capabilities report the dispatch binding version, installation and instance IDs,
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
Each descriptor carries its exact pinned profile reference, including for
explicitly disabled gates. The
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

An ordered request sends the existing seven strict binding headers with
`Bitrouter-Tenancy-Version: 2`, plus exactly one `Bitrouter-Candidate-Selection`.
The latter is strict JSON encoded as canonical, unpadded RFC 4648 base64url,
at most 4096 encoded bytes. Its decoded shape is:

```json
{
  "schema_version": 1,
  "admission_mode": "profiled",
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

The route MUST match the exact retained revision and body model. The
selection must match the authenticated profiled root. The accepted selection
becomes part of the receipt and usage evidence. The list has 1–64 unique
integer ordinals below 256, each present in that root. Distinct ordinals
preserve their own profile attachments even when they share a provider target.
Invalid or unauthorized selections fail before a provider send. Ordinary
dispatch rejects the selection header. The gateway removes it before dispatch.

Selection constructs an immutable request-local view. It narrows candidate
membership and required credential dependencies without changing the cached
revision, active configuration or another request. It never grants permission
to a selected resource. Only policy-free static generation roots without
learning, experiments, auxiliary counting, native harness, OAuth refresh or
continuation requirements qualify. Ordered continuation-bearing input fails
explicitly; the separately discovered provider-native opt-in below accepts
opaque controls without introducing owned-handle authority.
Unselected private credentials do not become usable through later hooks.

The selected order bypasses runtime sorting and weighted permutation. Existing
live rate/health admission and fallback classification still apply. A permitted
fallback can skip an ineligible candidate; actual attempts form a unique ordered
subsequence. Unknown transport/body/stream outcome, cancellation or completed
work MUST stop further attempts. Missing usage dimensions alone do not imply an
unknown execution outcome. All attempts share the admitted execution deadline;
fallback does not reset it or the separate stream-idle rules.

#### Receipts, bounds and recovery

Ordered receipts bind the accepted route, server-observed ingress protocol,
ordered selection and host attempt limit. Every actual attempt records its
selected ordinal and a required nullable observed HTTP status (200–599).
Completed attempts cannot claim non-2xx status. An observed 2xx status does not
prove completion. An actual unknown attempt requires an unknown receipt;
successful execution with missing usage can still be `incomplete`.

Ordered receipt JSON is bounded to 1 MiB. Let A be accepted selection length and
K = ceil(A / 8). Original usage evidence allows 512K frames, at most 128 per
actual attempt and 32 quantities per frame. Frame bytes are bounded to 192 KiB K,
with 64 KiB K reserved for identities and terminal metadata, for at most
256 KiB K total (2 MiB at A=64). This allocation depends on accepted candidates,
not eventual sends. Overflow retains a prefix and sticky truncation; it never
manufactures zero quantities or complete evidence. Ordered dispatch has no auxiliary children.

Before any effectful provider work, the owning writer commits a bounded dispatch
intent containing the immutable identity, selection and budgets. An uncertain
intent acknowledgement cannot authorize a send. Terminal receipt, original
evidence and intent retirement commit atomically. Startup terminalizes surviving
intents as unknown with unavailable observations before enabling dispatch; it
never replays inference. A missing receipt is not proof that no work happened.
The maximum receipt/evidence/disposition/intent obligation, including three row
charges, is 3,212,288 bytes and fits the existing 4 MiB execution reservation.
The receipt and evidence APIs serve both dispatch modes on the same paths.
An ordered receipt carries the accepted selection and original ordinals.
Ordinary dispatch retains its eight-attempt bound. Correlation pagination
preserves multiplicity across modes. Execution and correlation IDs confer
no receipt-read authority.

#### Request-local generation defaults

An ordered request can carry one bounded
[defaults envelope](generation-defaults-envelope.schema.json) in the
reserved body member `bitrouter_generation_defaults_envelope`. The object
requires `schema_version: 2`, `supplemental` and `configuration_defaults`.
The configuration choice is exactly `apply` or `preserve_absence`. Optional non-null `canonical` uses the
[canonical defaults schema](generation-defaults.schema.json). Supplemental
values can contain any JSON value allowed by the byte, nesting and
reserved-name limits. Duplicate fields at any depth fail before parsing
can erase them. The canonical fields are `temperature`, `top_p`, `max_tokens`,
`reasoning_effort`, `top_k`, `seed`, `stop`, `presence_penalty`, and
`frequency_penalty`. A single stop string normalizes to an array.
Caller values, including zero, take precedence. Typed null and an empty
stop list permit filling. An opaque source extra using a canonical name
can block filling without being forwarded across protocols.

Bind the envelope with the ordered dispatch binding and attach the same
envelope to the SDK request. Missing, mismatched, null or invalid
attachments fail before execution admission. Ingress removes the member
before passing the request to the inference protocol parser. Ordinary
dispatch rejects it.

The digest domain is `bitrouter.generation-defaults-envelope.v2` followed by
a zero byte, then compact UTF-8 JSON in exactly `schema_version`, optional
`canonical`, `supplemental`, `configuration_defaults` order. Supplemental
object keys sort recursively by decoded string order. Arrays preserve order
and numbers use Serde JSON representation. All defaults and the
configuration choice bind the digest, including values displaced by caller
input. Retain the digest in receipts and evidence without storing the
defaults payload there.

`apply` fills absent typed temperature and output tokens from retained
configuration. `preserve_absence` skips only that configuration-specific fill:
caller values (including zero) and canonical defaults still win, supplemental
values keep their existing provenance, and other retained defaults still apply.
The retained raw temperature and token aliases remain filtered, so they cannot
reintroduce the skipped layer. Chat and Responses omit absent typed values;
Messages supplies its existing required `max_tokens: 4096`, while omitting
absent temperature. Explicit later protocol requirements still apply. A null
input is not an assertion of key absence and keeps existing parser semantics.

This is a request-only selection, not a nullable configuration format or an
execution-token budget. Bounded ingress reservation precedes validation.
Execution admission, intent, RPM/probe and provider work have their own
checks. Ordered model-policy routes return `capability_mismatch`. Intent,
receipt and archive retain only the opaque digest; outstanding-intent recovery
terminalizes Unknown without storing or replaying the envelope or layer choice.

Only these exact top-level supplemental keys are reserved: `model`, `messages`,
`stream`, the nine canonical names above and
`bitrouter_generation_defaults_envelope`. There is no prefix blacklist and no
recursive name ban. Names such as `provider`, `headers`, `tenant_id`, `api_key`
and `extra_protocol` remain opaque provider JSON; they do not modify routing,
credentials, HTTP headers, provenance or receipt identity. The post-parse merge
writes only generation parameters, never a Prompt or authority/configuration
object. Provider-specific interpretation remains the provider's contract.

Apply canonical values first, then supplemental parameters, before retained
configuration defaults and ordinary ordered checks. A parsed caller extra of
the same name suppresses a supplemental default globally, including explicit
null; that caller extra still renders only on its source protocol. Existing typed
caller values, including false, win. Valid boolean `store` and
`parallel_tool_calls` fill their existing typed slots. Valid `stream_options`
fills the existing Chat-only typed options. Invalid/null special values fall
through to the supplemental map without coercion. Other fields, including raw
`tool_choice`, remain opaque supplemental values available to every outbound
adapter. Typed-owned wire fields win. Raw null/invalid special values do not
mask a later retained valid typed default; ordinary supplemental entries
inserted earlier win over later raw defaults.

`max_tokens` belongs in `canonical`. Residual `max_completion_tokens` and
`max_output_tokens` are not promoted, renamed or used as canonical admission
limits. Chat omits extra `max_completion_tokens` and retains its selected typed
token spelling; Messages can emit both residual aliases alongside required
`max_tokens`; Responses' typed `max_output_tokens` wins when present. Existing
retained canonical limits can therefore suppress a residual alias. These rules
preserve existing adapter behavior, including provider-specific omissions.
Streaming Chat forces `stream_options.include_usage=true`, replacing a raw
nonobject options value with an object; JSON rendering does not perform that
streaming transformation.

Ordinary ordered continuation checks remain mandatory after merge and during attach.
Effective non-null `previous_response_id`, `conversation`, `context_management`
or `cachedContent` refuses before provider I/O, including false, zero, empty
strings, objects and arrays. Null is not continuation authority. Store and all
other conversion/capability checks retain their existing refusal/fallback
classification; uncertain provider work never grants permission to resend.

The 16 MiB whole-request budget includes the caller body and envelope. The SDK
uses bounded serialization and the existing JSON recursion budget: the 128th
nested container is rejected, counting the enclosing request/envelope/map.
Programmatic supplemental values are checked before hashing. There is no new
key-count limit, retained defaults cache, receipt payload, attempt limit or
manifest limit; the existing 64-attempt/256-candidate bounds still apply.

#### Scoped Beta header preservation

The scoped v1 and ordered v2 SDK helpers provide the consuming
`with_anthropic_beta(&[reqwest::header::HeaderValue])` method. It replaces only
the current call's `anthropic-beta` fields. Values retain their bytes, duplicate
entries and order; comma lists are not split, trimmed, sorted or deduplicated.
An empty slice removes the header, while an explicitly empty field value stays
present. Invalid HTTP field-value bytes are rejected by `HeaderValue`
construction before any request. Shared client defaults and dispatch bindings
apply. Preservation applies to parsed field values, not header-name
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

1. Missing or disabled ordered discovery prevents ordered submission.
2. Different concurrent subsets/orders of the same retained root preserve
   their own membership, resource authorization and immutable ordinal mapping.
3. Counts 1, 8, 9 and 64 retain all actual attempts; 65 and a host limit below
   the submitted count reject before provider work.
4. Current revocation after queueing or between hops blocks the next send;
   selection and capability caches never stand in for current grants.
5. JSON and SSE for all three protocols preserve their terminals and original
   usage through HTTP/1.1 and HTTP/2 and the unified receipt and evidence reads.
6. Unknown outcomes, truncated observations, lost intent acknowledgement and
   crash recovery cannot authorize replay, a second attempt or fabricated usage.
7. Restart preserves the exact receipt and evidence bytes for both modes.
8. Repeated opaque Beta values remain isolated per call over HTTP/1.1 and HTTP/2
   for JSON and SSE; replacing or clearing them cannot change the binding,
   request body or another call's headers.

### Explicit provider-native controls on ordered requests

The default-off startup option `tenancy.ordered_native_controls` permits an
explicit caller opt-in to provider-owned state under the current selected
credentials. The operator accepts that principals using the same provider
account may share its state namespace. This option proves no historical tenant,
principal, credential or response ownership. Installations needing isolation
must keep it disabled or use provider credentials with genuinely isolated state.
The option is immutable for a serving instance; restart requires fresh discovery.

Authenticated `GET /control/v1/tenancy/ordered/native-controls/capabilities`
returns the closed [version-one descriptor](ordered-native-controls-capabilities.schema.json).
It binds installation, instance and capability revision, reports current authority
revision, and enables the mode only when operator policy and ordered dispatch
readiness both hold. The SDK
`ordered_native_controls_capabilities` read and `with_native_controls` helper
require matching discovery, with no automatic refresh, speculative dispatch or
downgrade. The observation is not an authority lease.

An ordered Responses or Messages ingress request opts in with exactly one
`Bitrouter-Native-Controls: provider_native_v1` header. Missing header preserves
ordinary ordered refusal. Unknown or duplicate values and ordinary dispatch
with this header fail before a provider send.
Core consumes the header; it never reaches the provider. Normal current caller
authentication, route/member/profile/credential authorization, defaults digest,
admission, deadline and receipt rules remain mandatory. Caller credential rotation
under the same principal and new per-call/configuration defaults are allowed when
current authority permits them. A raw control value does not select a
credential or a configuration revision.

Responses `previous_response_id` and `conversation`, and Messages
`context_management`, retain opaque JSON values and provider validation. The gateway forwards the literal
`brc_` prefix in this mode without invoking a
managed resolver or a fallback between modes. Values cannot choose endpoints,
headers, credentials or candidate order. No owned continuation handle is published,
imported or reconstructed, and no history is replayed. Ordinary gateway response
IDs retain their existing encoding; this mode does not make them resumable or
export a separate native-ID mapping. Provider success establishes no
prior ownership or usage facts. Generate Content `cachedContent` belongs to
the private operation below.

Source-native fields remain protocol-bound. The existing post-parse merge suppresses
a supplemental key when source extras already contain that key, including null;
this mode does not change that merge. Fields which remain in both maps use the
renderer precedence for each actual target: source presence (including null) wins
only on that source protocol. In that case a Responses source null masks a
supplemental prior-response value on Responses, but cannot mask that foreign value
on Messages. Incompatible targets are skipped before rate/health
admission and provider attempts, retaining original selection ranks, root ordinals
and profiles. If none qualifies the request fails; Core never strips a non-null
source control to manufacture fresh generation. This is an intentional safety
boundary rather than cross-protocol omission compatibility. Retained route
defaults containing native controls remain outside the ordered static-root
contract; request-local bound envelopes provide the explicit path.

Raw values supply no original-member affinity. The classified rejection
rules may permit the next selected compatible target; unknown transport/body/stream
outcomes, cancellation or completed work stop further sends. Rendering and the
authenticated outbound body are checked against effective controls. Receipts and usage evidence describe the admitted execution only. An opted-in
request without native controls may execute ordinarily.

### Private ordered Generate Content

The private ordered operation uses `GenerationProtocol::GenerateContent`
and the terminal marker `ProtocolEnd::GenerateContentFinished`. It requires
an operator-paired native adapter.

Authenticated `GET /control/v1/tenancy/ordered/generate-content/capabilities`
returns `OrderedGenerateContentCapabilitiesV1`. Its installation, instance and
capability revision must match the retained dispatch binding. `enabled` requires
ordered readiness and an explicitly paired operator adapter;
`cached_content_enabled` additionally reflects current provider-native policy.
The SDK's explicit `with_generate_content` observation enables this protocol,
not cache-state ownership or a discovery retry. Non-null effective
`cachedContent` also requires the independent provider-native header opt-in.
Source null can suppress a native default; it cannot erase a foreign supplemental
control. Core rechecks effective controls after the bound defaults merge.

The only data endpoint is `POST /v1/tenancy/ordered/generate-content`, with exact
media type `application/vnd.bitrouter.ordered-generate-content.v1+json` and no
query-selected mode. `OrderedGenerateContentRequestV1` is a closed envelope with
`schema_version: 1`, retained route resource ID `model`, boolean `stream`, and
`request`, an original native object without competing model, stream or dispatch
members. Model/mode authority and the existing digest-bound defaults/envelope
remain outside that original object. Its encoded bound is 16 MiB; programmatic
native values reserve both enclosing objects within the transport's default
128-container recursion budget and fail validation before serialization when
that budget is exceeded. Defaults retain their existing nesting budget.

The request object must come from the retained original native input, preserving
field presence, IDs, signatures, controls and provider data. A canonical policy
projection or a prior codec's rendering cannot reconstruct lost source data.
Authorized preset defaults remain a separate commitment; neither source-default
flattening nor a raw cache selector establishes historical credential ownership.
The reached paired endpoint uses protocol `ordered_generate_content`, a fixed
`/generate-content` path and the same native envelope with the reached provider
model. Current route/member/profile/credential authority and the exact method,
endpoint, mode and body commitment bind each signed physical attempt.

Native-to-native JSON preserves the full raw candidates array and native fields.
Every candidate must have a valid object/content shape and concrete terminal;
explicit candidate indices are unique nonnegative int32 values. A non-null error
or prompt-block envelope is refused. The canonical policy/foreign-projection view
selects only the first candidate and never merges candidates. Missing usage is
not rewritten to zero, and opaque native signatures/media remain under the
actual authenticated native response owner.

Native SSE retains full raw payloads without assigning an omitted index to zero.
The first nonempty candidate event locks the response to either an anonymous
singleton or explicit indexed mode. Anonymous mode stays one unindexed candidate;
indexed mode requires unique nonnegative int32 indices in every later candidate
entry, including singleton events. Null, fractional, negative, out-of-range and
mixed anonymous/indexed identities are invalid. One candidate's completion does
not complete or retarget another candidate. Missing/null/unspecified finish is
nonterminal until a concrete reason seals that candidate; a sealed candidate
allows only its index and optional identical finish, never content, new metadata
or a null/unspecified finish downgrade.

Initial native metadata is retained. Root metadata remains allowed while a
previous candidate is open or the event introduces a new explicit candidate.
Only when all prior candidates were sealed and no new candidate is introduced
are root tails restricted to usageMetadata, responseId, modelVersion, candidates
and neutral `error: null`. Non-null errors and prompt blocks always fail.
Present responseId/modelVersion must be nonempty nonnull strings and remain
stable globally. A bounded terminal group is held from the first concrete finish
until every observed candidate is sealed and HTTP EOF is clean. Only then may
`GenerateContentFinished` become visible. Reset, cancellation, malformed/error,
identity switches or conflicting tails never authorize a successful terminal or
another inference send.

Ordinary compatible candidates use the strict canonical codec; native-only
constraints exclude incompatible targets before rate/health admission. The native
paired path must retain signed text/media even where canonical deltas cannot
represent their metadata. It must not use a safe projection refusal to erase a
native feature. Actual accepted headers, outcomes, usage observations, deadlines,
receipt custody and original selected ranks retain the existing ordered rules.
Unsigned/unknown bridge outcomes and interrupted streams never authorize fallback.

Provision native routes through [resource provisioning](#resource-provisioning).
The route must carry exact credential and admission-profile attachments for
every compiled root and contain an explicit Generate Content root. Only that
root can target the operator-paired `ordered_generate_content` bridge.
A Chat grant cannot create native membership. Discovery and provisioning
grant no dispatch authority and do not qualify an adapter response.

Native requests without an explicit per-call defaults layer preserve absence.
An explicit defaults envelope retains its committed configuration-defaults
choice. The original native request and that defaults layer travel separately
to the reached paired adapter; current authority and the body commitment are
rechecked at the actual send.

### Exact retained admission-profile observations

The installation operator can inspect explicitly retained admission profiles
without attaching a route or configuration. The separate version-one reader is
advisory: it creates no rate entry, health entry, probe lease, reservation,
operation, execution receipt or dispatch authority. Existing manifest observation
fields and profile versions retain their original wire formats and semantics.

- `GET /control/v1/tenancy/admission-profiles/observations/capabilities`
- `POST /control/v1/tenancy/admission-profiles/observations/v1`

Both require the actual control operator with `control:tenancy` scope; tenant
credentials, including tenant administrators, do not qualify. POST requires one
`Bitrouter-Installation-Id` header. Discovery returns the actual installation and
permits omission of that header; a supplied header must match. Every response
uses `Cache-Control: no-store`. The POST is a read query with no operation ID or
mutation recovery semantics. SDK methods are
`TenancyClient::admission_profile_observation_capabilities` and
`TenancyClient::observe_admission_profiles`; neither retries, batches, discovers
routes nor falls back to another interface.

The closed contracts are
[discovery](profile-observation-capabilities.schema.json),
[request](profile-observation-request.schema.json),
[snapshot](profile-observation-snapshot.schema.json) and
[entry](profile-observation-entry.schema.json). All fields are required,
including nullable fields. Unknown and duplicate fields fail. Integer decoding
accepts exact integral decimal/exponent spellings without floating-point
rounding; fractions and values outside the declared bounds fail.

A request contains schema version 1, the exact expected authority revision and
1–256 distinct full operator-pool Policy ResourceRefs. Different pinned revisions
of one key are valid; duplicate full refs are not. Both original request bytes
and response bytes are limited to 1 MiB. Discovery reports actual
installation/instance/authority, enabled state, count/byte limits and the supported admission profile. Successful discovery reports enabled=true; no separate
server setting is introduced by this contract.

The snapshot preserves the exact request order, cardinality and full refs. It
adds actual installation, instance, authority and UTC observation time. Each
entry reports the current same-key profile revision, current member RPM/charged
count when configured, the explicit observation-group count when configured,
health and availability. Required null is distinct from a measured zero. If both
count fields exist, their values must agree: retained profile validation requires
the associations to name the same rate group. Rate-only, observation-only and
both-null configurations remain valid. Zero RPM is a valid rate refusal.

Both the pinned revision and its current head must exist in the
authenticated retained profile index and be nonrevoked. Unknown/nonprofile/revoked
pins or missing/invalid/revoked heads produce complete entries with null current
revision/rate/count, unavailable health and authority_denied availability. No
other profile is substituted. Current-head RPM rotation does not replace the
requested pin. Health uses the installation, pool and group identity. Disabled fields remain null/disabled;
an authorized unused runtime truthfully reports zero/healthy. The normal refusal
priority is circuit_blocked, rate_limited, capacity_denied, then eligible.
Capacity denial preserves actual fields and consumes no admission capacity.

One five-second management deadline covers authentication, bounded body collection,
decoding, one operational snapshot, encoding and final authority verification.
Read capacity and synchronous authority/runtime locks are acquired without an
unbounded wait. Contention or deadline exhaustion returns whole-read
CapacityExceeded; inaccessible/poisoned runtime or nonlive authority returns
AuthorityUnavailable. These read failures do not permanently fence the service.
An authority change returns RevisionConflict for the whole batch without retry
or rebasing. The runtime guard is dropped before final authority verification and
is never held over I/O. The profile index uses existing retained authentication
and reload rules; no per-read database decryption or earlier out-of-band tamper
detection is promised. Installation callers retain existing ControlAuth lifecycle
semantics, without a new durable operator registry.

SDK response validation binds installation, expected authority, exact refs and
order. Consumers additionally bind actual instance discovery and their own age,
clock-skew and deadline policy. Separate batches have separate sampling times;
no response extends a lease. Route/credential lifecycle, grants and reached-hop
checks remain required for execution. A successful profile read never establishes
an executable association or permission to send.

Conformance covers provisioning and restart; exact
256-entry completeness; current RPM rotation and pin/head revocation; strict
nullable/numeric/count consistency; operator-only direct/HTTP authorization;
H1/H2 SDK transport; transient lock/body-deadline refusal followed by recovery;
an authority publication between sampling and return; and read-only durable
and operational state.

### Execution usage evidence

Usage evidence preserves bounded technical observations from an exact
execution, independently of prices, charges or payable quantities.

| Method | Path under `/control/v1` | Contract |
| --- | --- | --- |
| GET | `/tenancy/usage-evidence/capabilities` | `UsageEvidenceCapabilities` |
| GET | `/tenants/{tenant_id}/executions/{execution_id}/usage-evidence` | `ExecutionUsageEvidence` |

The [evidence DTOs and validators](../../../crates/bitrouter-ai/src/tenancy/usage_evidence.rs)
describe the field catalog and cross-field constraints. `ExecutionReceipt` and
`ExecutionUsageEvidence` each have one wire shape with a required `dispatch`:
`{"mode":"strict"}` or `{"mode":"ordered","accepted_selection":{...},"attempt_limit":N}`.
Strict attempts require `candidate_index: null`; ordered attempts carry an exact
ordinal and form a unique subsequence of the original accepted selection.
Both retain actual attempt order and observed HTTP status. The evidence dispatch
must equal the receipt dispatch. `validate_against(&ExecutionReceipt)` binds
identities, dispatch and every captured attempt's target and outcome.

The SDK reads both modes through `execution(installation, tenant, execution)`
and `usage_evidence(&receipt)`. Strict evidence is limited to 256 KiB and 512
frames; ordered evidence reserves one such block per eight accepted candidates,
up to 2 MiB and 4096 frames. Original response bytes, including whitespace, count
toward these limits. Each attempt retains at most 128 frames. Ordered evidence
requires null parent and empty child linkage. Receipt and evidence payloads have
no integer schema-version field or alternate versioned read route.
Discovery requires authentication and `Bitrouter-Installation-Id`. Check
the advertised bounds, estimator and child-linkage support before consuming
the observations.

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
| `not_captured` | Historical receipt predating this capture contract; no original observations are asserted. This status grants no store authority. |
| `unavailable` | A version-1 admission seed proves capture was installed, but recovery lost the observations; retain an unknown outcome and durable unavailable evidence. |

Both absence states require empty attempts, frames and child IDs, null parent
and estimate, `estimation: not_captured`, `truncated: false` and
`dropped_frames: 0`. Current seeds retain their declared serialization and
absence semantics; unsupported capture versions and missing required seeds fail
validation. A current execution MUST NOT become `not_captured` after observation
loss. HTTP errors or an unsupported evidence endpoint do not assert either capture
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

Chat Completions may include `text_tokens` inside `prompt_tokens_details` or
`completion_tokens_details`. This capture compatibility case qualifies only a
redundant text-only breakdown: each present text counter MUST be an exact
nonnegative integer within the existing safe range and equal its corresponding
`prompt_tokens` or `completion_tokens` in the same observation. A present cache
read or reasoning counter respectively MUST be an exact in-range integer no
larger than that total. Present audio or image counters in the same detail object
MUST be exact zero; other modality or extension fields remain unsupported.
Missing text counters do not change existing capture behavior. Invalid or
contradictory redundant counters set `invalid` and make normalized projection
incomplete, while preserving independently observed quantities. In particular,
output text already includes reasoning; the two MUST NOT be added together.

Chat capture also recognizes these exact metadata paths, for JSON and qualified
usage chunks alike:

- Root `cost` is advisory provider metadata. If present, it MUST be a finite,
  nonnegative JSON number; malformed values set `invalid` and make normalized
  projection incomplete. Its units are not inferred. Its value MUST NOT enter
  quantities, tariff selection or charge calculation.
- `prompt_tokens_details.created_cache_tokens` is qualified only when it is the
  exact integer zero. This is a zero-only constraint, not a new cache-write
  quantity. Cache-write observations therefore remain absent. Positive, null and malformed values remain
  `unsupported`; this compatibility case assigns no positive cache pricing.
- `prompt_tokens_details.multimodal_tokens` is qualified only when null, meaning
  no multimodal input breakdown. Every non-null value, including an empty object
  or a populated zero-valued modality, remains `unsupported`.

The prompt-detail shapes are documented by
[vLLM's PromptTokenUsageInfo](https://docs.vllm.ai/en/stable/api/vllm/entrypoints/serve/engine/protocol/).
The advisory-cost acceptance rule is this extractor's explicit metadata policy,
not a provider currency or billing contract. These exceptions apply only to
Chat Completions at the stated paths; other protocols, paths and unknown fields
retain their existing refusal behavior. No metadata names or values are retained.

These validated redundant counters and metadata add no quantity, default
zero or retained field. Preserve retained evidence, flags and digests exactly.
An extractor revision does not qualify commercial usage.

Upstream usage field meanings remain defined by the
[Chat API reference](https://developers.openai.com/api/reference/resources/chat),
[Qwen Chat usage reference](https://help.aliyun.com/en/model-studio/qwen-api-via-openai-chat-completions)
(including the overlapping text/reasoning counters), and
[Messages streaming specification](https://platform.claude.com/docs/en/build-with-claude/streaming).
The extractor's qualified sources and bounded projection above do not extend
those upstream contracts.

#### Private ordered GenerateContent observations

Only the explicit private ordered `generate_content` ingress and paired native
outbound wire qualify this protocol. Chat permissions and ordinary endpoint
names do not authorize it. Ordered evidence uses
`extractor_version: 2` exactly when the accepted ingress is `generate_content`
or any retained frame has that protocol; otherwise it stays extractor 1.
Frameless or unavailable native ingress also requires extractor 2.

Native JSON and HTTP rejection frames use `json` and `http_error`; native SSE
uses `generate_content_chunk`. Each original `usageMetadata` object is a snapshot,
not an additive increment. The scalar mapping is `promptTokenCount` to input,
`candidatesTokenCount` to output, `totalTokenCount` to total,
`cachedContentTokenCount` to cache read, and `thoughtsTokenCount` to reasoning.
The [official UsageMetadata definition](https://github.com/googleapis/googleapis/blob/master/google/ai/generativelanguage/v1beta/generative_service.proto)
uses int32 counters: retained values must be exact nonnegative integers no larger
than 2,147,483,647. Missing quantities stay absent, explicit zero stays zero, and
malformed present quantities are null with `invalid: true`. Detail, tool or
modality fields outside this scalar mapping set `unsupported` without erasing
known facts. No implicit cache-write or reasoning zero is asserted.

Original output is the candidate-token counter; thoughts are a separate native
quantity. A normalized inclusive output requires both explicitly observed values
and a checked sum. Repeated cumulative snapshots must not be summed. These
technical observations do not qualify commercial settlement or certify complete
usage from an estimator, invalid stream, unknown outcome or missing dimensions.

#### Bounds, estimates and children

Ordinary dispatch allows 256 KiB of encoded evidence, eight attempts, 512
frames per execution, 128 per attempt, 32 quantities per frame and 64 child
IDs. Ordered bounds depend on selection size. Both count encoded whitespace. Capture retains an ordered
prefix. Once a frame cannot fit, truncation is sticky; `dropped_frames` counts
discarded observations with saturation at the exact integer maximum, and
`truncated` MUST equal `dropped_frames > 0`. Neither truncation, unsupported or
invalid data nor missing completion evidence establishes complete provider
totals or zero cost for unseen work.

`estimation` distinguishes unrecorded estimator provenance (`not_captured`),
affirmatively observed non-use of an estimator (`not_used`), and an estimate
actually used (`captured`). Non-use is recorded by the installed stream
finalizer or the successful non-streaming native generate-content executor,
which does not install that estimator. Absence of an observation MUST remain
`not_captured`; it MUST NOT be inferred to mean `not_used`. Only `captured`
permits a nonnull `estimate`.

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

### Optional provider technical facts

`GET /control/v1/tenants/{tenant_id}/executions/{execution_id}/provider-technical-facts`
uses the installation binding, current tenant-wide receipt-read or audit
authority, bounded read deadline and `Cache-Control: no-store`. The SDK entry
is `TenancyClient::provider_technical_facts`. The closed response is
`ProviderTechnicalFacts` or JSON null. An unknown execution returns
`not_found`. Null means the execution exists without retained measurements.
An invalid optional object fails only this read. A technical measurement
does not assign billing or authorize inference replay.

The [provider facts schema](../../../crates/bitrouter-ai/src/tenancy/provider_technical_facts/schema.json)
describes the single flattened response. The document binds to the exact
execution and receipt. `operations`, nullable `pipeline_scope`,
`hop_observations` and `observer_hops` are required fields in the same object.
The observer category records the independent local callback clock when
captured. Empty arrays and a null pipeline scope mean unobserved categories;
at least one category must contain observations. One 64 KiB bound applies to
the entire response.
Operations identify unique ordered subsequences of actual attempts, including
authentication recovery. Cross-check observed groups, ordinals and closing
attempts against the retained receipt. Provider/model labels and correlation
IDs do not establish timing identity.

The document has no `receipt_format` discriminator. Bind it with
`ProviderTechnicalFacts::validate_against(&ExecutionReceipt)`. The receipt's
`dispatch` limits strict executions to eight actual attempts; ordered attempts
must be a subsequence of the accepted selection, whose count cannot exceed
`attempt_limit` (at most 64). The facts' standalone 64-arm transfer bound does
not admit extra attempts: every referenced arm must occur in that validated
receipt. Accepted candidates that were never armed supply no observations.

#### Timing sources

Provider operation time covers the original monotonic operation, including
authentication recovery and response consumption. JSON truncates to whole
milliseconds. Stream times round a positive submillisecond sample to one.
First semantic output means nonempty reasoning, text, a tool name or arguments
before rendering hooks. Usage, metadata, terminal markers and tool IDs alone
do not count. No semantic sample yields null first and generation times. A
single semantic sample yields zero for generation time. Internal stream
collection for a JSON reply follows JSON timing and has no stream TTFT.
Independently rounded TTFT and generation durations need not add up to the
operation duration.

The nullable pipeline scope describes the actual owning pipeline context and
its settlement path. Positive submillisecond pipeline duration rounds up
to one. An observed stream uses the first opened upstream clock
through the owning processor's finalization and its accepted pre-hook deltas.
Across internal turns, the source auth arm and the selected terminal arm may
differ. Both must belong to the original receipt in order. Partial observations
cannot certify termination.

Hop observations describe each armed group and its actual callback boundary.
Failed-open and JSON callback guards, the successful opened-stream notify
scalar and observer-local callback clocks have separate meanings. A completed
wrapper without a successful provider terminal can still be partial. An
unobserved end has null duration and is partial. The
`core_json_hop_observer_floor_v1` and `core_stream_open_hop_observer_floor_v1` samples use the paired observer's
own start and end callbacks and floor the elapsed duration to milliseconds.
The observer runs first, so its sample includes subsequent start callbacks
and excludes subsequent end callbacks. Pairing is specific to a context and
fork. The second profile applies only to a failed stream open. Missing
callbacks or uncertain pairing omit the sample. A positive submillisecond sample may be zero.
A successful stream open uses the original `core_stream_hop_notify_v1` scalar
for its opened-stream hop.

A Core-owned clock cannot report upstream work performed in a paired external
bridge or merge observations across independent execution owners. Validate any
parent/child linkage against the actual usage evidence and each receipt. A
measured callback completion establishes neither complete delivery nor cost.

#### Retention

Capture is optional and runs outside required receipt or financial publication.
The original execution and shutdown take priority over the bounded capture
task. Missing measurements require no repair, replay or second write. Where
observer validation or the shared size bound prevents capture, omit that
category only if the remaining operation, pipeline and hop facts validate
within the bound. Corrupt retained facts fail the optional read; reads never
rewrite or repair the stored object. Where
an optional write outcome is uncertain, keep its bounded capacity charge until
it is safe to resolve. Exact-key readback validates immutable bytes and their
original scope. An optional capture failure cannot change receipt authority,
terminal evidence or startup eligibility. Optional objects install atomically
outside required schema initialization. Once written, facts read by their
original key and digest. Parent archival preserves the child bytes and
deleting the original parent cascades to the child.

### Resource and evidence migration and rollback

The service opens supported current runtime, tenant and shared authority
datasets. Each store is selected independently of software release and
Policy source versions. Archive incompatible datasets with their matching
binary and configuration before provisioning a new installation.

Store validation checks the expected runtime schema and its ownership
metadata. The SQLite tenant authority header binds an installation independently
of the database schema. Shared state requires the shared schema and
authenticated head and publication. A marker never replaces retained-content
validation.

Initialize current schema/baseline atomically in supported transactional SQLite
and PostgreSQL stores, only in genuinely empty locations. For generic runtime
backends with nontransactional DDL, including MySQL, incomplete initialization
remains nonready; publish the current ledger marker only after complete schema
validation under exclusive initialization custody. Never stamp/adopt a nonempty
old or foreign layout. Tenant service remains file-owned SQLite; shared authority
remains PostgreSQL. This does not add tenant/shared support to another backend.

Bootstrap, join, restart, restore and offline/import entry points validate the
current baseline and all retained affected source/envelopes, including inactive,
revoked, hidden and terminal history. A Policy-free active head cannot bypass
this check. Missing markers on nonempty stores, old/partial/foreign/future
layouts, forged current stamps and unsupported retained content are rejected
before listeners, claims, recovery, authority registration/heartbeat or mutations.
Physical locks/custody may be acquired for validation; this is not execution
authority. Shared refusal precedes Engine::join side effects. Failed opens never
clear/rewrite old databases or user files, or create a successful empty substitute.

An imported route or configuration must retain its exact current source,
bindings and credentials. Policy-bearing closures retain the model Policy
document and selected name. The selected route's ordinary, profiled or native
dispatch semantics survive publication and reopening.

Stop old ingress and finish resolvable work with the old deployment. Archive its
binary/data outside the new dataset. New instances use explicit new locations,
new installation identity and current APIs to provision resources; no implicit
copy/import of old rows. New binaries do not recover, resend, refund, fabricate
receipts for or claim completion of old paid work. Rollback runs the old binary
with its own consistent database/configuration/key copy, never a new database.

Current-format recovery is mandatory. Preserve no-send/observed/unknown dispatch
meaning, original receipts/output/usage and immutable bindings. Missing provider
evidence never implies success or zero usage. Missing terminal becomes Unknown
only with required exclusion, cleanup and stop-set proofs; otherwise refuse
ready/dispatch. Missing evidence from an installed capture remains unavailable.
Retain capacity/custody until settlement and cleanup acknowledgement. Begin,
rollback, commit errors, cancellation and lost acknowledgements retain unknown
obligations. The cumulative accepted-job count does not decrease after completion, failure,
cancellation or repeated recovery. Reduced builds refuse ready when they cannot service
current obligations. Recovery never replays providers or silently drops current
writers or obligations.

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
9. Absent provisioning discovery or `provisioning_enabled: false` prevents
   unsupported mutations. Invalid discovery grants no capability.
10. Current restart preserves revisions and evidence. Old/unmarked/partial/
    foreign/future stores and forged stamps fail before authority/provider work,
    including inactive Policy and Policy-free active heads. Refusal preserves
    records, file bytes and provider counts; old binaries refuse new formats.

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
8. Current capture recovery without observations persists `unavailable` with
   unknown outcome. Missing required seeds cannot become historical no-capture
   records; reject unsupported capture versions. Absence never asserts quantities.
9. HTTP/1.1 and HTTP/2 clients enforce the same bounds and exact
   installation/tenant/execution identities. Reads recheck receipt authority
   and return no-store responses; discovery alone grants no receipt access.
10. Advertised child linkage derives from independent admissions; parent and
    child ledgers remain separate without double counting. Generic server-tool
    capture stays unavailable.
11. An absent evidence endpoint is unavailable functionality. Required
    evidence cannot silently fall back to normalized receipt usage.
12. Current restart preserves receipt/evidence bytes and binding; old dataset
    upgrades and destructive downgrade are refused. Rollback uses the old binary
    with its own verified compatible data copy, without deleting bindings.

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
bearer credential before parsing queries or bodies. Inference and host
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

### Shared model-policy publication

`Publication.format` is exactly 2. Optional `model_policy` is a typed public
Policy Document; when absent no model Policy is installed. Unknown fields and unsupported formats return
400 `invalid_input` before mutation or head publication. ACL `policies` and principal `policy_id`
retain their independent access-control meaning and grants. Expected-generation
CAS, actor-bound replay, encrypted retained source, immutable generation and
current recovery guarantees remain required.

### Shared Operator API compatibility cases

1. An inference or host-control token cannot authenticate the operator
   listener. Insufficient grants fail before a mutation is applied.
2. Unknown query/body fields, non-JSON or encoded mutation bodies and oversized
   input are rejected. Responses include `Cache-Control: no-store`.
3. An exact actor/operation replay returns the original receipt without applying
   the command twice; changed input with the same identity conflicts.
4. A timed-out mutation remains unknown until a same-actor receipt resolves it;
   an unresolved receipt is not reported as rollback or successful execution.
5. Credential read projections do not disclose imported credential plaintext.
6. Reject unsupported publication formats and unknown fields before mutation.
   Typed source round-trips through the SDK, operator API, storage and restart.
   ACL policies stay independent.

## Eval exchange API

The [Eval specification](../eval/README.md) defines subjects, results and
snapshots. Gateway Eval uses the configured inference credential boundary,
including explicit development configurations that allow unauthenticated
requests. Tenant Eval uses the protected control listener.

| Method | Path under `/v1/evals` | Request | Response |
| --- | --- | --- | --- |
| POST | `/subjects` | `EvalSubject` | `PutSubjectReport` |
| GET | `/subjects` | — | Subject list |
| GET | `/subjects/{eval_id}` | — | `EvalSubject` |
| POST | `/results` | `EvaluationResult` | `AdmissionOutcome` |
| POST | `/snapshots` | Empty or `FreezeRequest` | `EvalSnapshot` |
| GET | `/snapshots/{evidence_root}` | — | `EvalSnapshot` |
| GET | `/status` | — | Ledger status |

Gateway errors use the Eval `{"error":"message"}` envelope. A result's evaluator
must match the host's configured authority, principal binding, kinds and metric
permissions. For either listener, resolve ambiguous write responses with a
read when possible. A network error cannot establish rollback.

### Tenant Eval profile

Every operation requires a tenant bearer credential and exactly one
`Bitrouter-Installation-Id` header. The specified installation, tenant path
and any submitted subject identity MUST agree. The credential and its current
binding must cover the whole tenant. State, expiry and grants are rechecked
after waits and before commit. A resource-scoped grant or installation-operator
credential cannot authorize this ledger.

| Method | Path under `/control/v1/tenants/{tenant_id}/evals` | Request | HTTP 200 body | Permission |
| --- | --- | --- | --- | --- |
| POST | `/subjects` | `EvalSubject` | `PutSubjectReport` | `evaluation.submit` |
| GET | `/subjects` | — | Subject list | `resource.read` |
| GET | `/subjects/{eval_id}` | — | `EvalSubject` | `resource.read` |
| POST | `/results` | `EvaluationResult` | `AdmissionOutcome` | `evaluation.submit` |
| POST | `/snapshots` | Empty or `FreezeRequest` | `EvalSnapshot` | Both |
| GET | `/snapshots/{evidence_root}` | — | `EvalSnapshot` | `resource.read` |
| GET | `/status` | — | Ledger status | `resource.read` |

These endpoints reject query parameters and duplicate JSON keys at every
nesting level. Subject/result POSTs and nonempty snapshot POSTs require JSON.
An empty snapshot request or omitted or null `frozen_at` selects the service
clock. Status reports the `heldout` count. Every response uses
`Cache-Control: no-store`. HEAD inherits GET authorization.

Manual result submission requires `kind: generic`, the authenticated
principal as `evaluator.authority_id` and no hard violations. Requests that
fail these requirements are rejected. Host evaluator configuration does not
grant trusted authority on the tenant API.

Result admission and any resulting disputes commit together. An exact
authorized result replay returns the existing outcome with `duplicate: true`.
Conflicting immutable content fails. A rotated credential for the same
principal may replay without changing creator provenance. Freeze atomically
selects and stores an immutable snapshot. Its timestamp is a label and
does not act as an as-of query or an idempotency key.

Tenancy errors have null operation and execution IDs and
`dispatch_state: not_started`. This dispatch state describes provider work.
The ledger may already have committed when the response cannot be delivered.
Invalid content is 400, invalid credentials 401, denied grants or inactive
tenancy 403, missing authorized facts 404 and installation mismatch 409.
Limits can return 413 or 429. Storage or integrity failure and uncertain
outcomes return 503. Clients MUST NOT retry an uncertain mutation implicitly.

Header bounds are 128 values and 64 KiB. Subject/result wire and canonical
payloads are bounded to 1 MiB and freeze requests to 4 KiB. Each collection or
conflict set is limited to 1,024 rows. The service bounds serialized work to
32 MiB and an encoded success response to 8 MiB. Overflow fails without
truncating the collection. Eight leases bound concurrent operations, including
body custody. Work has a five-second absolute deadline including queue time.
Precommit cleanup gets one additional second. Uncertain cleanup or commit
fences new authority while the actor owns the transaction.

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
values; authentication and framing overrides are rejected locally.
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
