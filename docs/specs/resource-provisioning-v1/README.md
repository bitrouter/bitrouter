# Resource provisioning v1

Status: draft. Specification revision: 1. Wire `format`: 1.

This contract provisions generic, immutable upstream credential, route and
policy resources over an authenticated control API. It does not define login,
commercial accounts, pricing, financial settlement or distributed authority.
MUST and MUST NOT describe required observable behavior.

## Authentication and discovery

Every request MUST authenticate with a control operator or a tenant credential.
Tenant credentials authorize only the intersection of their current permissions,
binding scope, expiry and tenant lifecycle. Explicit operator pools are separate
ownership namespaces. Tenant credentials MUST NOT mutate operator-pool resources.
An operator MUST NOT put one tenant's private dependencies into another tenant's
configuration. Upstream secrets are distinct from control/tenant Bearer tokens.

`GET /control/v1/tenancy/resource-capabilities`, with
`Bitrouter-Installation-Id`, returns `format`, `installation_id`,
`provisioning_enabled`, `max_command_bytes` and
`tenant_routes_require_approved_origins`. The descriptor is independent of the
strict tenancy v1 capability schema. Discovery MUST NOT be treated as a grant.

## Mutations

`POST /control/v1/tenants/{tenant_id}/resources` accepts a `ResourcePut`:

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

`expected_revision` is required. Null creates the remotely managed head;
otherwise it MUST equal the current managed head. The service assigns a fresh
immutable revision and returns the existing tenancy `OperationResult` shape.
Rotation MUST NOT rewrite an older definition or retarget an existing immutable
configuration. A manifest-only resource has no remotely managed head; creating
a managed revision of that key leaves existing manifest revisions unchanged.

Definitions are tagged objects:

| `kind` | Fields | Required permission on the addressed key |
| --- | --- | --- |
| `credential` | `secret`: 1–65,536 UTF-8 bytes | `credential.manage` |
| `route` | `routing`: explicit service routing object; `selector`; required nullable `policy`; `credentials`: provider/account/exact credential revision bindings | `resource.draft` |
| `policy` | `document`: explicit policy object; `name`; `dependencies`: exact resource references | `resource.draft` |

All referenced dependencies require current read authority; upstream credential
dependencies also require `credential.use`. Resource kind MUST match definition
kind. The complete serialized command MUST fit 1 MiB; credential bindings and
policy dependencies are limited to 256 each. Duplicate bindings/dependencies,
ambient providers, inline provider keys, host configuration fields and invalid
closures MUST be rejected. Routing and policy document compilation belongs to
the service and may reject facilities absent from the selected binary.

Tenant-defined route endpoints MUST use operator-approved exact origins. Tenant
definitions MUST NOT override authority with Host, authentication, forwarding or
hop-by-hop headers. Approval qualifies a retained immutable revision; changing
the provisioning-origin list does not revoke existing revisions. Operators MUST
revoke retained resources explicitly when withdrawing that approval. Operators
control approved origins and their DNS/network destinations. Operator-authored
routes retain the host's explicit upstream authority.

`POST /control/v1/tenants/{tenant_id}/resources/revoke` accepts `format`,
`operation_id`, `installation_id`, exact `reference` and
`expected_authority_revision`. Revocation requires `credential.manage` for
credential slots or `policy.activate` for other resources. It MUST compare the
observed authority revision. Both current and historical revisions can be
revoked. Revocation MUST fence subsequent use, including attempts through cached
routes, without deleting immutable evidence. Re-enabling requires a new revision
and explicit configuration selection; revocation cannot erase external effects
of a provider request already sent.

Resource changes, managed-head/revocation metadata and operation outcomes MUST
commit atomically before acknowledgement and authority publication. Requests
with the same principal and operation ID MUST return the original result for an
identical command, and `operation_conflict` for different content. Lost replies
are recovered through the existing operation lookup API with the original ID.
Caller cancellation MUST NOT create an alternative writer or lose an admitted
operation. Inference correlation remains unrelated to management idempotency.

## Reads and confidentiality

`GET /control/v1/tenants/{tenant_id}/resources` takes the five `ResourceRef`
fields as query parameters: `owner_kind`, `owner_id`, `resource_kind`,
`resource_id`, `revision_id`. `Bitrouter-Installation-Id` is required. Current
`resource.read` authority is checked before storage access and again before
returning the response.

The response contains `format`, `installation_id`, `reference`,
`authority_revision`, required nullable `head_revision`, and `revoked`.
Definitions and secrets MUST NOT be returned. Secrets MUST be encrypted at rest
with installation/reference binding and MUST NOT appear in operation records,
error messages or diagnostic formatting. A plain unkeyed secret digest MUST NOT
be retained as a command fingerprint.

## Compatibility and failures

Envelope fields are closed: unknown or duplicate fields and unknown enum values
MUST be rejected. Required nullable fields MUST distinguish omission from null.
Integer values MUST be exact within the tenancy number bounds. Peers MUST reject
unsupported format versions. Existing v1 tenancy discovery and execution receipt
schemas are unchanged; this API does not extend them implicitly.

Errors use the existing structured tenancy error vocabulary, including
`forbidden`, `not_found`, `revision_conflict`, `operation_conflict`,
`capability_mismatch`, `capacity_exceeded` and storage/authority failures.
Missing or invalid evidence of a management outcome MUST remain unknown until
the original operation can be queried. A caller MUST NOT recover uncertain
inference by resubmitting it.

See [conformance](conformance.md) for required observable cases.

JSON Schemas describe the [put command](put.schema.json),
[revoke command](revoke.schema.json), [status](status.schema.json), and
[capabilities](capabilities.schema.json). Schema validation is necessary but
does not establish permission, CAS, document compilation or conformance.
