# Control HTTP API v1

Status: **beta, specification revision 1.0.0-beta.1**. Wire namespace:
`/control/v1`. This draft records the implementation at revision
`80550c9b`; it is open to incompatible revision. The beta label does not
change the wire version or establish a stable compatibility promise. MUST,
MUST NOT, SHOULD and MAY have their usual normative meanings.

## Scope and discovery

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

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/control/v1/capabilities` | Caller-specific discovery |
| GET | `/control/v1/state` | Reload generation and configuration state when enabled |
| GET | `/control/v1/status`, `/models`, `/requests` | Status, models and request reports |
| POST | `/control/v1/route/preview` | Preview a route |
| GET | `/control/v1/providers`, `/observe/status`, `/policy/status`, `/policy/show`, `/agents` | Administration reports when enabled |
| POST | `/control/v1/reload` | Submit guarded reload when enabled |
| GET | `/control/v1/operations/{request_id}?instance={server_instance_id}` | Read the caller's reload operation |

`models` accepts the optional `provider` query. Filter and payload shapes are
provided by discovery and the [public types](../../../crates/bitrouter-ai/src/control/).
An endpoint's presence in this table does not guarantee it is enabled in a
particular distribution or granted to a caller.

## Authentication and isolation

Credentials are bearer tokens in `Authorization`. `control:read` grants
inspection; `control:reload` authorizes reload and requires `control:read`;
`control:tenancy` is separate and does not imply either. Host control tokens
are independent of inference credentials. The default legacy token grants read
only. The listener's deployment and TLS requirements are operator settings;
clients MUST authenticate the target and MUST NOT send tokens to an untrusted
host. Token configuration requires at least 32 UTF-8 bytes. Handlers recheck
scope even if discovery advertised an action.

## Guarded reload

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

## Errors and evolution

Control errors have `{"error":{"code":"...","message":"..."}}`.
Examples include 401 `unauthorized`, 403 `scope_denied`, 400
`invalid_request`, 404 `unsupported_action` or `operation_not_found`, 409
`stale_generation`, `server_instance_changed`, `reload_in_progress` or
`idempotency_conflict`, 410 `operation_expired`, and 503 `operation_capacity`.
Clients MUST handle unknown codes as failures and MUST NOT retry mutations
blindly. Inputs with unknown fields are rejected where the input schema says so;
clients SHOULD ignore unrecognized response fields and consult advertised
schemas. No extension or version negotiation beyond discovery is promised.
See [conformance](conformance.md).
