# Owned evaluation ledger HTTP transport

This additive control-listener contract exposes the existing evaluation ledger
DTOs in an authenticated tenant namespace when the `core` feature is enabled.
It does not change the management mutation bodies, sixteen management operations,
legacy `/v1/evals/*` endpoints, or public EvalStore methods.

## Authentication and installation binding

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

## Initial evaluator profile

Manual results must identify the authenticated stable principal exactly as
`evaluator.authority_id`, use `EvaluatorKind::Generic`, and contain no
`hard_violations`. Other evaluator fields remain submitted provenance. Requests
are rejected rather than rewritten. Host EvalConfig cannot confer a trusted rank,
metric wildcard, or permission. Full result-for-subject validation still applies.
Manual imports work with learning off and do not initiate providers or learning
jobs, consume evaluation-job allowances, or activate policies.

## Atomicity and retry semantics

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

## Bounds and failures

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
