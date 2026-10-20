# Shared gateway configuration

The default is `gateway_state: { mode: local }`. Local SQLite requires no
coordinator or deployment key. `multi-instance` is opt-in and implies
`db-postgres`; it does not distribute agent execution or harness sessions.

Shared mode requires `server.skip_auth: false`, a named PostgreSQL database,
an explicitly initialized namespace, and an environment reference for the
deployment's base64-encoded 32-byte key. Replicas use the same namespace and key.
Do not put the key itself in configuration or substitute it into the reference.

```yaml
server:
  skip_auth: false
database:
  url: ${GATEWAY_DATABASE_URL}
gateway_state:
  mode: shared
  namespace: production-gateway
  deployment_key_env: GATEWAY_DEPLOYMENT_KEY
  pool_connections: 16
  transaction_timeout_ms: 3000
  owner_lease_ms: 30000
  heartbeat_ms: 5000
  local_max_in_flight: 256
  worker_interval_ms: 1000
  worker_batch_size: 64
  drain_timeout_ms: 30000
  max_request_bytes: 4194304
  max_connections: 256
  header_timeout_ms: 30000
  request_timeout_ms: 30000
  write_stall_timeout_ms: 30000
```

The owner lease must exceed three heartbeat intervals plus one transaction
deadline. Database connections are bounded to 1–128, each background pass to
1–256 records, and local in-flight requests to 1–65536. Excess admission is
rejected without an unbounded waiting queue. Request bodies are bounded to
1024–16777216 bytes. An expired owner lease does not prove provider work stopped.

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
