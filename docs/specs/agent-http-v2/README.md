# Native Thread HTTP API v2

Status: **beta, specification revision 1.0.0-beta.1**; wire namespace
`/agent/v2`. This draft describes the opt-in implementation at `80550c9b`;
beta revisions may change incompatibly. MUST, MUST NOT, SHOULD and MAY express
requirements on peers. It is independent of ACP wire v2.

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
| POST `/threads/{id}/steer`, `/inputs`, `/resume` | Steer, answer input, or resume queue |
| GET `/threads/{id}/history`, `/observe` | Paginated committed history or SSE observations |

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
operations MUST NOT be inferred from their path alone. See [conformance](conformance.md).
