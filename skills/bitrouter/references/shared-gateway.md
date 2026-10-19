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
```

The owner lease must exceed three heartbeat intervals plus one transaction
deadline. Database connections are bounded to 1–128, each background pass to
1–256 records, and local in-flight requests to 1–65536. Excess admission is
rejected without an unbounded waiting queue. Request bodies are bounded to
1024–16777216 bytes. An expired owner lease does not prove provider work stopped.

Routing and policy authority is published separately from these process-local
settings. Replicas must never substitute their own provider credentials or policy
files for unavailable shared authority. Do not enable shared mode on a database
holding unrelated local continuation/trajectory authorities.
