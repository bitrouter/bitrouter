# Shared gateway configuration

The default is `gateway_state: { mode: local }`. Local SQLite requires no
coordinator or deployment key. `multi-instance` is opt-in and implies
`db-postgres`; it does not distribute agent execution or harness sessions.

Shared mode requires `server.skip_auth: false`, a named PostgreSQL database,
an explicitly initialized namespace, and an environment reference for the
deployment's base64-encoded 32-byte key. Replicas use the same namespace and key.
Do not put the key itself in configuration or substitute it into the reference.

Use the official `gateway-shared` archive for your target, or build with
`cargo build --locked -p bitrouter --bin bro --no-default-features --features multi-instance`.
`bro --json build-info` must report `profile: "gateway-shared"` and the sorted
features `["db-postgres", "multi-instance"]`. The `gateway` distribution still
has no optional application features; `full` retains its existing defaults.
The shared capability does not select shared mode without explicit configuration.
For manual archive verification and capability-preserving updates, see
[updating](updating.md).

Run `bro serve --config <file>`. Shared
mode is a foreground service managed by the operator; it does not use the
local daemon launcher, PID/socket locator, home directory or compiled extension
host. It never initializes or publishes a namespace as a startup side effect.
Routing, policies and provider credentials must already be published there.
`bro start` and `bro restart` reject shared configuration before local daemon
discovery or runtime-file creation. Use your service manager to run and stop the
foreground process; do not use the local daemon launcher for a shared deployment.

```yaml
server:
  listen: 127.0.0.1:4356
  skip_auth: false
database:
  url: "${GATEWAY_DATABASE_URL}"
gateway_state:
  mode: shared
  namespace: production-gateway
  deployment_key_env: GATEWAY_DEPLOYMENT_KEY
  pool_connections: 16
  transaction_timeout_ms: 3000
  owner_lease_ms: 30000
  heartbeat_ms: 5000
  local_max_in_flight: 256
  max_retired: 4
  worker_interval_ms: 1000
  worker_batch_size: 64
  drain_timeout_ms: 30000
  max_request_bytes: 4194304
  max_connections: 256
  header_timeout_ms: 30000
  request_timeout_ms: 30000
  write_stall_timeout_ms: 30000
```

The selected document is read once before local startup, with a 16 MiB input
limit. Select `gateway_state.mode` literally; environment interpolation cannot
select or change the mode. Shared documents accept `server.listen`,
`server.log_level`, `server.skip_auth`, `database.url`, `gateway_state` and optional
`telemetry` and `operator`. Explicit `inherit_defaults: false` and `registry: {enabled: false}`
are accepted. Provider/model/policy, control, harness and plugin configuration
belongs outside this process document and is rejected before environment lookup.
Quote environment references in shared YAML strings. Bounds must be literal
numbers. Shared value substitution cannot inject new YAML fields.

Telemetry is disabled unless this process document contains `telemetry`. Its
fields use the built-in OTLP configuration, and `OTEL_*` overrides are resolved
once. It requires an OTLP feature and does not load local account attribution or
create an installation ID. The process owns one exporter across routing changes.

Readiness requires a live original owner lease, fresh access through the main
database pool and a loaded snapshot matching the active authority. An expired
owner never silently rejoins. SIGINT/SIGTERM starts one fixed drain deadline;
another termination signal selects force while retaining task joins. Forced
shutdown preserves uncertain custody/native records for recovery and exits
unsuccessfully if work remains unresolved. Local mode keeps its daemon workflow.

The owner lease must exceed three heartbeat intervals plus one transaction
deadline. The total database connection budget is 2–128: one is reserved for
heartbeats and the remaining connections serve requests and other workers.
Each background pass is bounded to
1–256 records, and local in-flight requests to 1–65536. Excess admission is
rejected without an unbounded waiting queue. Request bodies are bounded to
1024–16777216 bytes. An expired owner lease does not prove provider work stopped.

`max_retired` accepts 1–64 (default 4), in addition to the current generation
and one candidate/build slot. At the limit, configuration replacement waits for
resource reclamation; it never evicts a generation with outstanding work. A
cancelled or failed candidate retains its slot until its build and cleanup are
confirmed complete. Readiness still requires the loaded authority revision to
be current.

Shared assembly rejects MCP servers, active server tools, decision-model
execution, request checkers and the agent API. These execution capabilities
remain local until they have explicit shared worker ownership.

Shared guarded policies retain the episode's original correlation key across
rotation. Native continuation pins must match the committed guard's selected
tier, model and reasoning effort; a conflicting pin is rejected before provider
dispatch. Local capture/evolution controller headers and local reserved judge
requests are unsupported. Access policies cannot use local spend or rate counters;
configure shared admission limits instead.

Run the same shared-gateway build on every replica in a namespace. Rolling
replacement is supported within that build; mixed versions of private durable
records are not a supported upgrade protocol. Changes to those records require
a coordinated stop and a compatible migration before restarting readers,
workers and operator services. Do not roll back to a reader that rejects newer
records after the newer writer has accepted work.

Optional action observations describe canonical callback content at execution
scope. They are separate from the original route decision and do not establish
provider success, quality, usage or latency. Missing callbacks, a crash before
terminal persistence, or unresolved provider work can leave these observations
absent; recovery does not reconstruct them from a current cache.

Shared HTTP serving uses one request per HTTP/1 connection, with no upgrades or
HTTP/2. Clients must reconnect for subsequent requests. This bounds response-head
accumulation while a peer stops reading, at the cost of connection reuse. Local
mode retains its existing HTTP transport. Connections and unreaped connection
tasks are capped together at `max_connections` (1–65536); overload closes the
new socket without queuing a task.

Header, body-ingress and write-stall deadlines each accept 100–300000 milliseconds.
The ingress deadline ends when the body is consumed and does not limit inference
duration. Write-stall timing starts only on output backpressure; reads and no-op
flushes do not extend it. Positive output progress permits a longer stream.
Outgoing DATA frames are copied into chunks of at most 64 KiB; response metadata
is bounded separately. These are transport bounds, not an exact process RSS
budget or a guarantee that the peer received a completed response.

Routing and policy authority is published separately from these process-local
settings. Replicas must never substitute their own provider credentials or policy
files for unavailable shared authority. Do not enable shared mode on a database
holding unrelated local continuation/trajectory authorities.

## Operator listener

Shared process configuration can explicitly enable an independent operator
listener. Omitting this block creates no operator listener or fallback credential:

```yaml
operator:
  listen: 127.0.0.1:4358
  max_connections: 16
  credentials:
    - id: service-operator
      token_env: SHARED_OPERATOR_TOKEN
      grants: [read, policy_write, credential_write, publish]
```

The address must be numeric loopback with a nonzero port. Use a TLS proxy or
tunnel for remote access. Supply a separately generated high-entropy token in
the named environment variable (32–4096 Bearer-compatible bytes). Actor IDs,
token variable names and grants are literal process inputs; keep actor IDs
consistent across replicas so receipt recovery retains its identity. Tokens are
loaded before database pools open and never come from inference or legacy
control authentication. Token rotation requires restarting the process with the
same actor identity. The process validates the complete block before resolving
environment references; an explicit empty or malformed block is an error.

The listener serves `/operator/v1` separately from inference. Explicit grants
are `read`, `policy_write`, `credential_write`, `publish`, `rotate_correlation`,
`resolve_execution` and `global_principal_write`. The last capability covers
database-wide principals; ordinary namespace read/write grants do not imply it.
Write-only operators can recover their own command receipts using the command's
grant. No namespace or actor override is accepted in a request.

Use the SDK's explicit `operator::OperatorClient` for mutations, receipt lookup,
revision and metadata reads. Operation IDs are query values on
`GET /operator/v1/operations?operation_id=...`; mutations use
`POST /operator/v1/mutations`. Preserve the original ID after unknown outcomes.
Operator connections are bounded to 1–256 (default 16), JSON requests to 2 MiB,
and the entire handler to `gateway_state.request_timeout_ms`. The backend also
checks its encoded actor-plus-mutation size, so passing the wire byte limit alone
does not guarantee acceptance. Content encodings are unsupported. Header and
write-stall limits remain the shared transport limits.

Operator requests do not depend on the loaded inference generation's readiness
during operation. Normal startup still requires valid initialized authority and
successful initial assembly; this is not an admin-only rescue process. Both
listeners drain or force-stop under the same process deadline.

## Remote operator commands

Use a build with `multi-instance` and the separate operator token:

```sh
bro shared apply --endpoint http://127.0.0.1:4358 \
  --token-env SHARED_OPERATOR_TOKEN --input mutation.json
bro shared operation --endpoint http://127.0.0.1:4358 \
  --token-env SHARED_OPERATOR_TOKEN --operation-id original-id
```

The apply input is one literal JSON `Mutation`, bounded to 2 MiB. `--input -`
reads stdin. It contains `operation_id` and a `command` with `action`/`input`:

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

Each command requires explicit `--endpoint` and literal `--token-env`; there is
no named context, local configuration, database or daemon fallback. Token values
are never command arguments. HTTPS is required remotely; loopback HTTP supports
a local TLS tunnel. Invalid endpoint/token rejects before input reads. The input
is not an environment template and can contain write-only secret material; retain
it privately with the original operation ID. Apply sends once. On an unknown
outcome, query the original ID with the same actor; never invent a new recovery ID.

`operation` returns a committed receipt or `unresolved`. Both successful reads
exit zero and emit one JSON value; unresolved does not prove rollback. Failed
lookups do not settle earlier writes. `--human` renders the same distinction.
An output failure after a known commit does not repeat the mutation.

The same endpoint/token flags support `revision`, `tenant --id ID`,
`resource --id ID`, `principal --id ID`, `credential --provider ID --account ID`,
and `execution --id ID`. These read current metadata without exporting secrets,
polling, repeating inference or expanding IDs into URL paths.

## Explicit initialization

Use a build with `multi-instance`:

```sh
bro shared init --config PROCESS.yaml --plan SEED.json \
  --operator-id bootstrap-admin --operation-id initial-seed
```

The PostgreSQL database and deployment key are explicit prerequisites. The
command initializes both schema ledgers, then commits a complete seed and its
receipt in one transaction. It starts no listener, serving incarnation or local
daemon. Neither a named context nor implicit home/config discovery is used.
`--plan -` reads stdin. Success emits a secret-free JSON receipt; `--human` emits
its namespace, audit identity and original committed generations.

The JSON seed has `resources`, `tenants`, `principals`, `credentials` arrays and
a required `configuration` object. Resource/tenant entries use their operator
put shapes with `expected_revision: null`; principal entries use
`expected_authorization_revision: null`; credential imports use
`expected_version: null`. All are unique creates. Referenced tenants, resource
pools and credential slots must be included in this seed. Configuration uses
`format: 1`, explicit `routing`, embedded `policies`, optional `policy_lock`,
credential bindings and `require_known_pricing`; it contains no process settings.
Inputs are literal JSON, with no environment or file expansion inside the seed.

A minimal seed for verifying installation and replica readiness is:

```json
{
  "configuration": {
    "format": 1,
    "routing": {},
    "policies": [],
    "policy_lock": null,
    "credentials": [],
    "require_known_pricing": false
  }
}
```

This seed publishes no model routes or inference principals. For inference,
include the resource pools, tenants, principals, credentials and routing bindings
in the initial seed.

Include `--global-principals` only when explicitly creating database-wide virtual
principals. Each input contains a lowercase SHA-256 verifier of a client-generated
high-entropy inference secret, not its plaintext. Provider credential imports are
write-only secret inputs. Keep the original plan available privately for recovery;
the receipt never exports these secrets or verifiers.

The whole plan is bounded to 2 MiB and 64 operations including publication. The
configured database transaction deadline also bounds the seed transaction.
Failed pre-commit domain writes roll back together; completed schema migrations
remain. A same-identity, identical-plan retry returns the original successful
receipt. It does not replay changes over later administration. Different input
under an already committed initialization, or a namespace initialized by another
operation, conflicts. An uncertain response is not proof of rollback: reuse the
same operator ID, operation ID and plan to recover its receipt. Ordinary `serve`
continues to require a published namespace and never performs this initialization.
