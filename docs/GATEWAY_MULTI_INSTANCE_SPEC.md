# Native gateway multi-instance implementation

Status: implementation contract; delivery is not yet accepted.
Updated: 2026-10-20. P0 base: `9cbe512d101e777470d113b0086ab9e331b3a638`
([#984](https://github.com/bitrouter/bitrouter/pull/984)).

This implements the complete P1 milestone in the
[service refactor plan](BITROUTER_SERVICE_REFACTOR_PLAN.md#p1-native-gateway-multi-instance-operation).
The [implementation ledger](GATEWAY_MULTI_INSTANCE_PROGRESS.md) records actual
completion separately. A database URL alone does not enable shared authority.

## Product and ownership

Several independent official gateway processes serve one logical gateway behind
an ordinary load balancer. Requests, continuation lookup, execution queries and
admission do not require affinity. Live response streams belong to their original
process. This does not distribute the core scheduler or move harness session/tool
ownership into the gateway.

- `bitrouter-ai::gateway` owns credential-free HTTP contracts, including durable
  execution status and operator acknowledgement shapes. It gains no transport,
  database, conversion or asynchronous runtime dependency.
- `bitrouter-sdk` supplies explicit gateway and operator clients. It does not
  retry inference automatically, execute tools, implicitly acknowledge unknown
  provider work, or accept/provider-export secret credentials in its DTOs.
- `bitrouter::gateway::shared` owns PostgreSQL authority, admission, execution
  records, recovery, delivery and shared secret storage. Existing gateway
  execution, metering, continuation and trajectory modules retain their domain
  ownership and participate in this authority.
- The opt-in `multi-instance` feature implies `db-postgres`. Local SQLite remains
  the default mode and requires no shared service, key, worker or coordinator.
  Local builds without this feature keep their dependency closure. Official
  distribution must expose a usable shared-gateway profile, without custom host
  assembly or editing source code.

## Explicit deployment and authority

`gateway_state.mode` is `local` by default. `shared` requires PostgreSQL, a
nonempty cluster identity, a 32-byte deployment key supplied through an explicit
environment reference, bounded local admission and database timeouts. It also
requires real inference authentication (`server.skip_auth: false`). Unsupported
or conflicting authority configuration is rejected before listeners start.

An operator explicitly initializes a shared namespace. Joining replicas never
silently publish their own local route configuration or generate independent
continuation/correlation authorities. The shared schema has a separate migration
ledger and is initialized only for shared operation; existing local migrations
and local database contents are preserved.

Local process configuration owns listener addresses, database connection details,
deployment-key references and worker bounds. A versioned, validated shared
snapshot owns provider/account bindings, model/router/policy definitions and
published guard configuration. File-backed policies and policy-lock data must be
included in that snapshot rather than read independently by each replica.
Environment-derived provider secrets are imported into encrypted shared storage;
they are not re-resolved from each worker's environment during inference.

Shared snapshot preparation currently rejects `google_extensions: true` on Chat
models. That extension's tool replay proofs require static target credentials;
the shared credential-version binding does not yet provide that replay contract.
Ordinary Chat Completions and local Google extension behavior are unchanged.
This is an explicit shared-mode compatibility limit, never a silent downgrade.

Configuration publication uses compare-and-set against the current generation.
A replica prepares a complete immutable routing/policy snapshot and exposes it
atomically. Every new admission checks the authoritative generation; a stale or
unavailable snapshot produces a bounded 503, never a request authorized against
an independent stale policy. Already admitted work retains its recorded routing
snapshot. A generation change before an attempt's dispatch authorization rejects
that attempt with `authority_changed`; it does not silently reroute the accepted
execution. Already dispatched attempts may finish. Credential/principal
authority is checked again at provider dispatch.
Readiness reports the active/loaded generations and backend accessibility.

Tenant identity is the authenticated virtual key's owning user, never a caller
supplied tenant header. Principal identity is the virtual-key ID. The existing
`api_keys` authority is read from the database; its effective authorization
revision is a content digest of activation, expiry, owner, policy and limit
fields, computed from the locked database row. Shared tenant/resource policy
has an explicit revision. Admission and dispatch-intent writes use serializable
transactions: hold a shared row lock on the active configuration head, then lock the original
API-key row, then tenant, execution, selected resource/credential and attempt
rows in that order (sort multiple resource IDs). The authoritative revisions
must match those used to prepare the decision. Existing API-key writers update
that same row; shared policy/credential writers lock and advance the corresponding
authority row. Serialization failures retry only the local transaction with a
bounded budget, never provider I/O. Commit is the admission/dispatch boundary.
Configuration publication locks that head exclusively. Concurrent admissions may
hold shared locks together, but publication waits for their short transactions;
a transaction that encounters an already committed publication must revalidate
or reject. Thus stale dispatch authorization cannot commit after publication.
Serializable isolation alone does not establish this commit-order guarantee.
A changed generation or authorization rejects un-dispatched work; a later change
does not retroactively prove that already dispatched provider work has stopped.

## Request identity and HTTP behavior

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
See the [SDK usage and recovery contract](../crates/bitrouter-sdk/README.md).

## Shared admission

The initial hard resource dimensions are tenant concurrent executions, tenant
requests per database-clock minute, and actual provider-account/resource-pool
concurrent attempts. Admission can additionally be restricted to an explicit
tenant resource-pool set. Limits and bindings are revisioned shared authority;
aliases for the same upstream account consume the same resource, not separate
per-provider-name counters. A credential replacement cannot silently change the
resource charged by an existing attempt.

Admission reserves tenant capacity and an idempotency binding atomically. The
provider resource is reserved after route/credential selection, before dispatch.
Rejection of this first tenant-admission transaction changes neither execution
nor counters. The accepted execution consumes one rate-bucket unit even if later
preparation or provider-resource admission fails. Such a later failure commits a
terminal pre-dispatch rejection, releases tenant concurrency and preserves the
idempotency binding; it does not refund the accepted-request rate unit. A crash
in this gap is recoverable as an abandoned, un-dispatched execution. A duplicate
during the gap observes the accepted execution and consumes no extra capacity or
request-rate quota. Lowering a limit below current occupancy
blocks new work while retaining truthful existing occupancy.

The local semaphore bounds work waiting on shared storage and slow response
consumers. Its acquisition is nonqueueing; excess work receives 429/503 with a
bounded retry hint. Database pool acquisition, transactions, result sizes,
worker batches and retries are bounded. Shared hard limits never fall back to
process-local counters on an unavailable backend. Existing policy limits must
either use the shared authority with their documented semantics or fail closed
as unsupported; no local hard-limit implementation may silently remain active
in shared mode. Operator business billing and account-balance policy remain
outside this milestone.

Shared inference bounds decoded upstream response bytes to 16 MiB per attempt,
including JSON, Decisions and the complete SSE stream. A narrower native request
bound still applies. Exceeding the bound rejects delivery; it does not authorize
replay of a dispatched attempt or discard an already validated terminal fact.
Hidden HTTP retries and redirects are disabled for shared sends. Input-count
requests owned by the core controller are outside this gateway milestone.

Transactions take the concurrent shared publication fence and lock only the
affected tenant/execution/resource rows exclusively, in a fixed order. They end
before credential refresh, inference, stream delivery or webhook I/O. No exclusive
gateway-wide request mutex serializes inference. PostgreSQL
[row locking](https://www.postgresql.org/docs/current/explicit-locking.html#LOCKING-ROWS)
and [transaction isolation](https://www.postgresql.org/docs/current/transaction-iso.html)
define the serialization points; database time supplies lease/rate decisions
instead of trusting replica wall clocks.

## Durable execution and recovery

An execution is accepted, running, unknown, or terminal. Attempts distinguish
prepared reservations, dispatch intent, observed provider terminal evidence and
unknown outcomes. Persist dispatch intent before the first upstream byte can be
sent. Do not hold a database transaction while sending it.

| Failure boundary | Required durable result |
| --- | --- |
| Before an attempt's dispatch intent | Recovery can prove no provider work was authorized; terminate abandoned preparation and release its reservation |
| After dispatch intent, before a provider terminal is durably recorded | Mark unknown after the owner lease expires; retain associated capacity and avoid inference replay |
| After durable provider terminal, before execution finalization | Reconcile from the stored evidence; commit terminal effects once |
| During streaming or after client disconnect | Preserve observed usage/terminal evidence and distinguish interrupted delivery from unknown provider execution |
| After terminal commit, before client observes success | A duplicate/query observes the committed execution; do not call the provider again |

Owner leases are fencing tokens and liveness evidence, not proof that an upstream
request stopped. An expired lease alone cannot release an in-flight/unknown
provider permit. Late terminal evidence may reconcile the original attempt,
but cannot mutate a different execution or reverse a committed terminal result.
Network ambiguity prevents automatic fallback/replay of that ambiguous attempt.
Known pre-dispatch rejection and observed provider rejection remain distinct.

Terminal accounting, reservation release and terminal outbox insertion share one
transaction and stable execution identity. Conflicting terminal evidence is an
error; the same terminal evidence is idempotent. Missing usage/cost remains
unknown, not zero. Existing metering/trajectory projections must be reconstructible
from committed evidence, so a crash between projection writes does not lose
accounting or reset guard authority. Reconstruction alone is insufficient:
terminal commit must also install a durable pending-projection fence for its
bound episode. The next guard transaction locks that episode's fence, applies
pending terminal facts idempotently, and advances the fence together with its
history/guard decision. A failed catch-up rejects the decision. Terminal commit
and guard transactions use the same fence before episode mutation, so releasing
capacity cannot expose stale cost/hold state to another replica. Successful
delivery requires the authoritative terminal commit and all existing required
continuation finalizers; a swallowed best-effort settlement error is not this
durability boundary.

An explicitly authorized operator can resolve an unknown attempt with a bounded
evidence reference and expected revision. The audit records the operator's
assertion as such; it does not masquerade as a provider observation. Resolution
never dispatches another provider request. Ordinary inference keys cannot use
this management authority.

## Credentials and continuation

The shared secret store encrypts provider credentials and service-private keys
with authenticated encryption bound to namespace, record identity and revision.
Secret values never enter public execution DTOs, outbox payloads or diagnostics.
Configuration/status APIs expose references and revisions only.

OAuth refresh acquires a durable, fenced account lease in a short transaction,
records refresh intent before network I/O, and persists the rotated replacement
before exposing it. A stale owner cannot overwrite a newer credential. An
ambiguous refresh after a process crash requires reconciliation/replacement;
lease expiry must not blindly replay a possibly consumed rotating refresh token.
Extend the credential transaction contract with an awaited pre-refresh transition
and explicit completion/release. `OAuthSession` must persist that fenced intent
before calling the refresher, including its rejected-token recovery path. A
network error after intent leaves an ambiguous refresh state; a replacement is
staged and durably committed before the lease can be released for future refresh.
Account refresh does not hold a transaction or row lock over the network.

All replicas use the same durable continuation key authority and credential
account identity. Existing encrypted continuation publication/delivery rules
remain applicable. A new replica must resolve an established continuation using
shared state alone. A changed account, unknown key or unavailable shared store
fails closed rather than silently starting a new provider conversation.

## Trajectory identity, key epochs and guards

Store the correlation key ring and its revision as shared encrypted authority.
New requests load/check the authoritative epoch. Rotation retains keys needed
by existing episode histories; lookup considers retained epochs and continues
an existing episode under its original canonical identity. A new active key
cannot split an existing episode or erase its active hold. Keys cannot be
retired while referenced history or an outstanding execution still depends on
them. The ring contains at most 16 retained epochs; rotation refuses when that
bound cannot be met without dropping referenced history.

Persist the execution-to-trajectory binding (owner, episode ID, canonical request
ID and correlation epoch) in the same transaction as the trajectory start.
Settlement and recovery use this binding, not a newly computed ID under the
currently active key. Resolve native parents and prefixes across retained epochs
before choosing the episode's original canonicalizer. Multiple different episode
matches are a conflict, not permission to select one or start an empty history.
The existing single-epoch correlation batch receives one consistent selected
epoch; mixed-epoch digests are not passed through its current validation.

Native-parent and canonical-prefix lookup remain tenant scoped. Unknown epochs,
conflicting history, missing prefixes or a stale authority read fail closed;
none may turn a previously guarded trajectory into an empty successful history.
Use the existing transactional correlation/start/route-intent batch and sequence
reservation, extending its authority checks where required. Settlement reduction
must use the committed current episode head. Guard decisions and history appends
are serialized per episode, never for the whole gateway and never across inference.

## Outbox, draining and bounded operation

Terminal events have stable IDs, immutable content and bounded payloads. Delivery
workers claim bounded batches with fenced leases, release database transactions,
then deliver. A receiver may see the same ID more than once. Acknowledgement is
idempotent; delivery attempts do not charge usage or finalize execution again.
An unavailable receiver does not require unbounded in-memory buffering.

On shutdown, stop new admissions/readiness, drain accepted local work within the
configured bound, and persist known terminal evidence. Work cut off by a hard kill
is recovered using the table above. A slow client cannot retain an unbounded
queue or cause stream migration. Cluster/outbox workers are owned tasks with
explicit cancellation and joins. Backend failure makes shared readiness false
and rejects new shared admissions, while existing work retains truthful state.

Shared serving initially permits one request per HTTP/1 connection, with no
upgrades or HTTP/2. This deliberate connection-reuse limit prevents accumulated
early-response heads from escaping Hyper's DATA buffering threshold. Local mode
retains its existing transport. Reuse can be added only with a proved finite
response-head/flush bound and corresponding slow-reader regression coverage.
The additional connection setup cost belongs in P1 process-load acceptance.

`max_connections` defaults to 256 and bounds both live connection permits and
unreaped task records. Admission is nonqueueing. The connection owner holds its
permit through Hyper's final flush and shutdown, independently of request-body
or execution-custody lifetime. The listener closes before draining; cancellation
aborts and joins outstanding connection tasks after the configured drain bound.
Application-spawned inference/finalization tasks remain covered by their own
custody and drain owners.

Shared transport uses a 64 KiB buffering threshold and copies outgoing DATA
into independently owned chunks no larger than 64 KiB, including small slices
of larger producer allocations. At most one original source frame/remainder is
held by the splitter, under request custody. Metadata limits count repeated
header values and reserve generated framing overhead; oversized heads fail
before encoding, oversized trailers terminate the body. This is not an exact
RSS budget: retained read capacity, framing/threshold overshoot, application
serialization allocations, task state and OS buffers remain separate costs.

Header, body-ingress and write-stall deadlines default to 30 seconds and accept
100–300000 milliseconds. The ingress deadline ends at consumed body completion;
it is not an inference deadline. Known oversized lengths are rejected before
service dispatch, and collector size hints cannot exceed the ingress byte limit.
Write timers start only on actual backpressure. Repeated Pending, reads and
no-op flushes do not extend the timer; positive write progress does. A healthy
long-running stream is allowed, without a peer-receipt or bounded-total-duration
claim. Real pinned-Hyper tests must cover body EOF before a blocked final flush,
single-request handling of pipelined early responses, both vectored and flattened
writes, ingress timeout separation and owned overload/shutdown behavior.

## Implementation stages and evidence

1. **Authority foundation:** configuration/feature boundary, public execution
   contracts, shared-only migrations, bounded PostgreSQL transactions and secret
   storage. Verify namespace isolation and rollback using a real PostgreSQL server.
2. **Admission and execution:** atomic tenant/idempotency/resource decisions,
   fenced attempts, terminal commit and recovery. Exercise independent connections
   and competing workers, including every crash boundary.
3. **Gateway integration:** attach these decisions to all served inference paths,
   actual selected credentials and settlement/delivery. Add execution queries and
   explicit SDK submission behavior without inference replay.
4. **Published authority:** operator API/CLI, atomic configuration generation
   changes, principal checks, credential import/refresh and continuation sharing.
5. **Trajectory authority:** shared key epochs, rotation-safe correlation, guards
   and settlement reconciliation; alternating/concurrent replicas must agree.
6. **Delivery and operations:** outbox workers, readiness, bounded overload,
   draining/rolling restart, documented official build/distribution profiles.
7. **Acceptance:** independent-process PostgreSQL campaigns, fault injection,
   complete feature/local regressions, controlled local performance comparison,
   independent aggregate review and a PR stacked on #984.

Each stage receives an independent review. Completion requires all original
P1-A01–A09 gates, not only store-level tests. The process campaign uses at least
two separately launched gateway executables, one counted mock provider, a real
dedicated PostgreSQL database and ordinary HTTP clients. Faults include worker
kill, database loss, failed delivery ACK and stale configuration/credential/key
snapshots. The local campaign uses SQLite with shared services stopped and
preserves the established development-build performance methodology and accepted
P0 overhead limits. No production throughput claim follows from fixture results.
