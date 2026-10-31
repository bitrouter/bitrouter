# Resource tenancy HTTP API v1

Status: **beta, specification revision 1.0.0-beta.1**; wire namespace
`/control/v1`. This draft describes the checked-in implementation at
`80550c9b`, not an assurance that all builds, backends or deployments support
it. Capability discovery and feature/platform restrictions remain authoritative.
Beta revisions may change incompatibly. MUST, MUST NOT, SHOULD and MAY express
requirements on conforming peers.

## Transport and authority

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

## Operations

All paths below are relative to `/control/v1`. Payload structures and field
limits are defined by the [JSON Schema](../../contracts/resource-tenancy/schema.json)
and the [public DTOs](../../../crates/bitrouter-ai/src/tenancy/). A schema-valid
request may still fail semantic, authorization or capability checks. The
[fixture set](../../contracts/resource-tenancy/fixtures.json) includes shape and
semantic negative cases.

| Method | Path | Result |
| --- | --- | --- |
| GET | `/tenancy/capabilities` | Capabilities and installation identity |
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

## Roles, scope and version binding

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

## Identity, replay and failures

An operation ID is bound to installation, authenticated principal and typed
command. An exact authorized replay returns the recorded result; reuse for a
changed command conflicts. Current authorization is rechecked before revealing
a recorded result. Credential issuance returns a secret only in the initial
successful response; replay and lookup MUST NOT reveal it. On a lost issuance
response, recover the ID/status and revoke or rotate as appropriate. Correlation
IDs are not idempotency keys. Results are retained for 30 days; after expiry the
operation ID remains reserved as a tombstone. The installation has bounded
operation capacity and a reserved safety pool. Saturation is a durable fence,
not permission to recycle IDs. A principal can recover its own operation only
while current authority covers the operation's saved scope.

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

## Receipts and pagination

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

## Tenant Eval profile

When `core` and the tenant Eval transport are available, the protected listener
also offers `POST/GET /tenants/{tenant_id}/evals/subjects`,
`GET /subjects/{eval_id}`, `POST /results`, `POST /snapshots`,
`GET /snapshots/{evidence_root}` and `GET /status`, with the latter paths
relative to `/control/v1/tenants/{tenant_id}/evals`. Tenant-wide authorization
is required. Responses including errors use `Cache-Control: no-store`.
An exact authorized result replay reports `duplicate=true`; changed immutable
content is rejected. Snapshot membership and root are frozen together. A
waiter timeout or postcommit authorization failure does not establish rollback.
The [operator profile](../../RESOURCE_TENANCY_EVAL_HTTP.md) gives bounds and
current feature limitations.

## Evolution and conformance

The schema's own `additionalProperties` rules govern unknown request fields;
clients SHOULD tolerate additive response fields but MUST reject unsupported
required semantics. No stable compatibility guarantee is claimed in beta.
[Conformance cases](conformance.md) cover the authority and retry boundaries.
