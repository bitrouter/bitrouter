# BitRouter service API

Status: **beta, specification revision 1.0.0-beta.1**. This is the single
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

Changes must review old-client/new-server and new-client/old-server behavior.
Do not silently change a field's type, units, default, nullability, enum meaning,
identifier scope, error semantics, terminal event or fingerprint encoding.
Capability discovery is required where offered; an unknown optional response
field can be ignored, but unsupported required behavior must fail visibly.
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
`server_version`, `actions`, `action_descriptors`, `resources`, `limits`,
`scopes` and optionally server and artifact identities. Descriptors contain
per-action input/output JSON schemas. Implementations and clients MUST tolerate
optional capability fields absent from older peers; an unknown action MUST NOT
be invoked merely because its identifier is recognizable.

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

### Authentication and isolation

Credentials are bearer tokens in `Authorization`. `control:read` grants
inspection; `control:reload` authorizes reload and requires `control:read`;
`control:tenancy` is separate and does not imply either. Host control tokens
are independent of inference credentials. The default legacy token grants read
only. The listener's deployment and TLS requirements are operator settings;
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
   request and response schemas. A client handles a missing optional descriptor.
3. Submit a valid reload using the sampled instance/generation. Disconnect and
   look up the same request ID on the same boot; the operation continues.
4. Replay the identical body under the same credential and verify no second
   reload is admitted. Reuse the ID with changed input: 409 conflict.
5. Stale generation, another boot and concurrent reload do not start a second
   operation. Unknown/expired results are not represented as a confirmed rollback.
6. A partially applied operation is a terminal report with participant outcomes,
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

The configuration and its referenced route/policy revisions are pinned for
execution. Current health, credential and authority changes can narrow eligible
targets, not expand an admitted envelope. Suspended or deleting tenants cannot
start or wake work; already admitted work can still finalize under its existing
ownership. A receipt-only audit reader may inspect retained receipts without
gaining mutation or execution authority.

### Identity, replay and failures

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

### Receipts and pagination

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

### Evolution and conformance

The schema's own `additionalProperties` rules govern unknown request fields;
clients SHOULD tolerate additive response fields but MUST reject unsupported
required semantics. No stable compatibility guarantee is claimed in beta.

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

## Shared operator API

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
See the [operator guide](../../../skills/bitrouter/references/shared-gateway.md#operator-listener).

### Shared Operator API compatibility cases

1. An inference or legacy-control token cannot authenticate the operator
   listener. Insufficient grants fail before a mutation is applied.
2. Unknown query/body fields, non-JSON or encoded mutation bodies and oversized
   input are rejected. Responses include `Cache-Control: no-store`.
3. An exact actor/operation replay returns the original receipt without applying
   the command twice; changed input with the same identity conflicts.
4. A timed-out mutation remains unknown until a same-actor receipt resolves it;
   an unresolved receipt is not reported as rollback or successful execution.
5. Credential read projections do not disclose imported credential plaintext.

## Eval exchange API

The optional `core` feature exposes the evaluator-neutral evidence exchange.
These endpoints append and read evidence; they do not publish a policy lock or
start a model invocation. Authentication follows the configured gateway
credential policy, including explicit development `skip_auth` configuration;
production clients MUST use the host's configured authentication boundary.

| Method | Path | Body / response |
| --- | --- | --- |
| POST, GET | `/v1/evals/subjects` | `EvalSubject` / insertion report or subject list |
| GET | `/v1/evals/subjects/{eval_id}` | Subject |
| POST | `/v1/evals/results` | `EvaluationResult` / admission outcome |
| POST | `/v1/evals/snapshots` | Optional `FreezeRequest` / frozen snapshot |
| GET | `/v1/evals/snapshots/{evidence_root}` | Frozen snapshot |
| GET | `/v1/evals/status` | Ledger status |

Fields, identity, numeric units, result and error envelopes are defined by the
[public Eval types](../../../crates/bitrouter-ai/src/eval.rs). Results refer to
subjects; evidence snapshots represent frozen membership, not live policy.
Subject and result identity are immutable: matching replay is distinguished from
conflicting data. Observation of transport success or cost does not by itself
establish task quality. Clients MUST handle authentication, invalid input,
missing subjects and server failures as failures, and MUST NOT use a network
failure as proof that a prior write did not commit. Unknown optional response
fields may be ignored; unknown required semantics MUST NOT be guessed. See the [operator workflow](../../../skills/evaluating-bitrouter-routes/references/eval-exchange.md).

### Tenant Eval profile

This additive control-listener contract exposes the existing evaluation ledger
DTOs in an authenticated tenant namespace when the `core` feature is enabled.
It does not change the management mutation bodies, sixteen management operations,
legacy `/v1/evals/*` endpoints, or public EvalStore methods.

#### Authentication and installation binding

Every operation requires a tenant bearer credential and exactly one
`Bitrouter-Installation-Id` header, including POST. Existing Eval request DTOs
remain unchanged; the installation header is the binding for this new transport.
A body field, query parameter, public caller context, host inspection credential,
installation operator, audit credential, or legacy virtual key cannot replace it.

The credential and its current recipient binding must both cover the whole
path tenant. Current credential state, expiry, binding, tenant state and permission
intersection are checked again after storage waits and before commit. Resource-only
grants cannot use this tenant-wide ledger. Operator pool grants are not required.

| Method | Path under `/control/v1/tenants/{tenant_id}/evals` | Request | HTTP 200 body | Permission |
| --- | --- | --- | --- | --- |
| POST | `/subjects` | `EvalSubject` | `PutSubjectReport` | `evaluation.submit` |
| GET | `/subjects` | None | `Vec<EvalSubject>` | `resource.read` |
| GET | `/subjects/{eval_id}` | None | `EvalSubject` | `resource.read` |
| POST | `/results` | `EvaluationResult` | `AdmissionOutcome` | `evaluation.submit` |
| POST | `/snapshots` | Empty body or `FreezeRequest` | `EvalSnapshot` | Both permissions |
| GET | `/snapshots/{evidence_root}` | None | `EvalSnapshot` | `resource.read` |
| GET | `/status` | None | `StatusReport` | `resource.read` |

No query parameters are accepted. POST subjects/results and nonempty snapshot
bodies require JSON. Duplicate JSON keys are rejected at every nesting level.
Malformed nonempty snapshot bodies and unknown FreezeRequest fields are errors;
only an empty body or an omitted/null `frozen_at` chooses the service clock.
All responses, including errors, carry `Cache-Control: no-store`. HEAD inherits
GET authorization. Status retains the existing `heldout` count key.

#### Initial evaluator profile

Manual results must identify the authenticated stable principal exactly as
`evaluator.authority_id`, use `EvaluatorKind::Generic`, and contain no
`hard_violations`. Other evaluator fields remain submitted provenance. Requests
are rejected rather than rewritten. Host EvalConfig cannot confer a trusted rank,
metric wildcard, or permission. Full result-for-subject validation still applies.
Manual imports work with learning off and do not initiate providers or learning
jobs, consume evaluation-job allowances, or activate policies.

#### Atomicity and retry semantics

A result, every resulting prior-result dispute, and its terminal admission event
are written in one transaction. Exact authorized replay returns the existing
latest outcome with `duplicate=true` and appends no events. Changed immutable
content is rejected. Missing admission closure is an integrity failure, not an
automatic repair. Rotated credentials for the same principal can replay without
changing creator provenance.

Owned snapshot freeze selects membership, computes the existing tenant root,
persists that exact manifest and validates its stored winner in one stable
transaction. `frozen_at` labels the manifest; it is not an as-of query or a client
idempotency key. A supplied `frozen_at` is validated as RFC3339 and its exact
decoded string is preserved in the manifest, reply and root calculation; there
is no timestamp normalization. An absent or null value chooses the actor clock
once for that operation. Historical snapshots retain their immutable closure
after later admission changes. Legacy root bytes and public freeze wrappers are unchanged.

The existing authority actor serializes ledger writes with management operations.
A revoke acknowledged before a job executes wins. A revoke queued during a ledger
transaction runs after that transaction. Expiry before commit dispatch causes
explicit rollback; expiry during COMMIT can have an unknown durable outcome.
Postcommit revocation can suppress a success response without removing evidence
that already committed. Bytes already handed to transport cannot be recalled.

#### Bounds and failures

The endpoint bounds headers to 128 values / 64 KiB, subject/result wire and
canonical JSON to 1 MiB, freeze bodies to 4 KiB, each collection/conflict set to 1,024 rows,
cumulative serialized SQL/manifest/write work to 32 MiB, and an encoded success
response to 8 MiB. Collection overflow fails instead of truncating. These are
payload/work bounds, not a claim about allocator overhead or process RSS.
Eight ingress leases cover body collection, queued work and response-body custody.
Work has a five-second absolute deadline including queue time. Precommit cleanup
gets one additional second from its first stop/error; missed cleanup fences new
authority while the actor retains the transaction. In-flight commit at the work
deadline fences immediately. No transaction is aborted or retried automatically.

Failures use the existing tenancy Error envelope with null operation/execution
IDs and `dispatch_state=not_started`. That dispatch state describes provider
work, not whether a ledger transaction committed. Invalid input/content is 400;
invalid credentials are 401; denied profile/grants or inactive tenants are 403;
wrong installation binding is 409; authorized absence is 404; oversized bodies
are 413. Confirmed ordinary capacity rejection is 429. Storage/integrity failure,
uncertain cleanup, or a response waiter timing out after enqueue is 503. A waiter
timeout is never evidence of rollback. No automatic retry is performed.

Already admitted execution observations use the same actor lane under existing
execution custody and their own bounded deadline. Their failure is retained and
makes shutdown unsuccessful even if database rollback succeeded. Cleanup does
not require a newly granted manual evaluation permission. This transport does
not enable production dispatch, strict receipts, or distributed writers.

### Eval Exchange API compatibility cases

1. An authenticated producer stores a subject and a result and reads only
   evidence available to its configured owner. A missing subject is not success.
2. An identical result replay is distinguished from changed immutable content.
3. A frozen snapshot remains bound to its original evidence membership after
   new results are submitted.
4. Posting evidence does not publish or mutate the active routing policy.
5. A lost response does not prove that a subject or result was not committed.

## Thread API

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
| POST `/threads/{id}/steer`, `/threads/{id}/inputs`, `/threads/{id}/resume` | Steer, answer input, or resume queue |
| GET `/threads/{id}/history`, `/threads/{id}/observe` | Paginated committed history or SSE observations |

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
operations MUST NOT be inferred from their path alone.

### Thread API compatibility cases

1. The dedicated bearer is required; capabilities identify the serving instance
   and do not advertise disabled native execution operations.
2. A request targeting an unauthorized workspace or a stale server instance is
   rejected. A client rediscovers after restart before a new mutation.
3. Matching mutation/key replay does not execute twice; changed content using
   the same key conflicts. Missing, non-ASCII or oversized keys fail.
4. Enqueued turn admission is not completion. Disconnecting observation does
   not cancel accepted execution; history remains accessible with authority.
5. An SSE error and a truncated stream do not become a successful terminal.

## Managed Core API

An active virtual service key authenticates `GET /v1/orchestrator/capabilities`
even if ordinary inference is configured to skip authentication. Availability
depends on the host's `core` feature and enabled service. The response identifies
the core instance, supported operations/transports and limits. A client MUST
honor the advertised capabilities rather than assume the endpoint's presence
implies every operation is enabled.

`GET /v1/orchestrator/channel` upgrades to WebSocket only with
`Bitrouter-Beta: orchestrator_core=v1`. Managed `POST /v1/responses` uses the
same beta header; the ordinary Responses profile remains separate. The caller
must first bind or restore a session over the core channel within the host's
initial-message deadline. The harness owns durable checkpoint persistence and
must explicitly acknowledge the checkpoint only after persistence; HTTP/SSE
observation does not acknowledge it. DTOs and message variants are defined by
[core protocol](../../../crates/bitrouter-ai/src/core/protocol.rs),
[checkpoint](../../../crates/bitrouter-ai/src/core/checkpoint.rs) and
[managed Responses](../../../crates/bitrouter-ai/src/core/responses.rs).

A channel disconnect or dropped HTTP stream does not itself complete or cancel
a run. No automatic replay or exactly-once result after uncertain delivery is
promised. A client MUST use the existing operation inspection/recovery semantics
before deciding on a new operation. `CoreError` carries `code`, `message` and
`commit_status` (`not_committed`, `committed`, `unknown`); `unknown` MUST NOT be
interpreted as rollback. Strict input structures reject unknown fields where
specified by the DTO. Unknown required capabilities or future message variants
MUST NOT be guessed.

### Managed Core API compatibility cases

1. Require a valid service key for capabilities even when ordinary inference
   skips auth. Reject an unsupported beta value before channel upgrade.
2. Require bind/restore within the initial deadline; unsupported operations
   fail rather than being inferred from a successful handshake.
3. Persist a checkpoint before sending its explicit ACK; observing HTTP/SSE
   output alone does not acknowledge a checkpoint.
4. Dropping the channel or observation does not establish cancellation or
   rollback. Treat `commit_status: unknown` as an unresolved outcome.
5. Reject malformed strict message input and unsupported required semantics.

## Native ACP extensions

### Connection and capability discovery

`bro acp serve` starts the native stdio bridge; `bro acp serve <agent>` is the
separate external-agent proxy. A native bridge connects to a local daemon; its
stdio carries ACP protocol messages. Native mode needs an explicit model or
`chat.model`. Initialization negotiates supported ACP v1 or v2 for the
connection; unsupported versions and unsupported methods fail visibly.
Consumers MUST use the advertised capability set instead of assuming that
optional methods exist. The native adapter's `_meta.bitrouter.version` is `1`;
its `schemaVersion` is `1.10.2`. `nativeExecution` reflects the `core` build
feature. Queue resume and close are advertised only when supported.

### Session and execution

An ACP session identifies a native thread. Supported lifecycle operations
include initialization, new, list, prompt, cancel, and version-appropriate
load/resume; close and queue resume depend on advertised native execution.
The v1 prompt returns its result when the turn settles. The v2 prompt returns
acceptance with the admitted user message identity and completion is projected
through updates. Consumers MUST NOT treat acceptance as successful completion.

A client disconnect detaches transport; it does not cancel accepted work.
Cancellation cancels active work and leaves accepted unstarted work in a
paused queue. Explicit close cancels active and queued work while retaining
history for authorized reopening. Pending approval is native state and may be
reattached on reconnect to the same live daemon; transport request IDs do not
confer approval authority. The advertised `_bitrouter/session/resume_queue`
extension resumes a paused queue subject to current authorization. Client info,
requested working directory and possession of a session ID do not grant access.

### Operation identity

Retry-capable clients may supply `_meta.bitrouter.idempotencyKey` for durable
operations. The key is scoped to the caller, Thread and operation; JSON-RPC
request IDs are transport identities, not idempotency keys. A matching retry
returns the original receipt and changed content conflicts. A client that lost
an acknowledgement without a reusable key SHOULD inspect authoritative history
rather than automatically submit a new prompt. Load or resume reattaches; it
does not itself resume a paused queue. A busy or queued prompt is rejected
rather than silently reordered.

### Validation and evolution

Invalid members in `mcpServers`, `prompt` or `additionalDirectories` are rejected
rather than dropped. Non-null `_meta` must be an object. A peer MUST NOT assume
that enabling draft v2 enables every draft extension, or that a response from a
future schema revision has the same meaning. Unknown optional metadata MAY be
ignored; unsupported required capabilities MUST fail visibly. See the [native adapter](../../../apps/bitrouter/src/harness/acp/native).

### Native ACP extensions compatibility cases

1. Initialize v1 and v2 separately and verify version-specific capability
   advertisement. Unsupported versions and methods fail rather than downgrade.
2. For each enabled version, create a session and prompt. In v2, acceptance is
   not reported as completion before the terminal update.
3. Disconnect after admission; reconnect with current authority and observe the
   same session without canceling the accepted turn.
4. Cancel active work and verify accepted queued work stays paused. Close an
   enabled session and verify active and queued work are canceled but history
   remains available to an authorized client.
5. Reject an invalid array member instead of silently dropping it. A session ID
   or client-provided working directory does not bypass authorization.
6. With core disabled, do not advertise native execution/queue controls that
   depend on core. Approval reattachment must not create a second approval.
7. Retry the same durable operation with its namespaced key and verify the
   original receipt; a different payload with the same key conflicts.

## Shared request identity

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
values; authentication and managed-profile overrides are rejected locally.
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
