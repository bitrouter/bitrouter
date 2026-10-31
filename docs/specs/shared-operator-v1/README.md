# Shared gateway operator API v1

Status: **beta, specification revision 1.0.0-beta.1**; HTTP namespace
`/operator/v1`. This profile records the optional multi-instance implementation
at `80550c9b`; it can change incompatibly while in beta. It is separate from the
[control API](../control-v1/README.md) and the [tenancy API](../tenancy-v1/README.md).
MUST, MUST NOT, SHOULD and MAY express requirements on peers.

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
See [conformance](conformance.md) and the [operator guide](../../../skills/bitrouter/references/shared-gateway.md#operator-listener).
