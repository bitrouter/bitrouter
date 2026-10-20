# Shared gateway configuration

The default is `gateway_state: { mode: local }`. Local SQLite requires no
coordinator or deployment key. `multi-instance` is opt-in and implies
`db-postgres`; it does not distribute agent execution or harness sessions.

Shared mode requires `server.skip_auth: false`, a named PostgreSQL database,
an explicitly initialized namespace, and an environment reference for the
deployment's base64-encoded 32-byte key. Replicas use the same namespace and key.
Do not put the key itself in configuration or substitute it into the reference.

Run a build with `multi-instance` using `bro serve --config <file>`. Shared
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
`telemetry`. Explicit `inherit_defaults: false` and `registry: {enabled: false}`
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
