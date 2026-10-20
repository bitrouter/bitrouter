# Native gateway multi-instance implementation

Status: implementation contract; delivery is not yet accepted.
Updated: 2026-10-21. P0 base: `9cbe512d101e777470d113b0086ab9e331b3a638`
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

- `bitrouter-ai::gateway` owns HTTP contracts, including credential-free durable
  execution/read projections and explicit write-only administrative inputs. It gains no transport,
  database, conversion or asynchronous runtime dependency.
- `bitrouter-sdk` supplies explicit gateway and operator clients. It does not
  retry inference automatically, execute tools, implicitly acknowledge unknown
  provider work, or export stored provider credentials or deployment secrets.
  Explicit credential-import inputs are separate from secret-free read and
  receipt DTOs; ordinary gateway requests never acquire operator authority.
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

`bro shared init` is the explicit local bootstrap entry. It requires a shared
process configuration, bounded JSON seed plan, operator identity and caller-owned
operation ID. It rejects named contexts and unavailable builds before discovery
or input reads, and rejects local configuration before expanding any local
environment references. Plans are literal JSON, not environment templates. The
operator and operation handles are validated before opening files or stdin.
The command uses one database connection and never joins a serving incarnation.

The seed is limited to 64 operations including publication and 2 MiB of encoded
input. Accepted file/stdin input is limited to 2 MiB; reads are capped at
2 MiB + 1 byte to detect overflow. Resources, tenants,
principals and credential slots must be unique creates, with references confined
to the seed. Principal creation additionally requires `--global-principals`
because virtual keys are database-wide. The complete plan, grants and publication
are prepared before any schema work. Both ordinary and shared migration ledgers
are installed under the shared initializer's transaction advisory lock. This
does not change the existing local migration entry point.

After schema installation, a single bounded transaction creates namespace
authority/keyring, applies the seed, publishes generation one and commits every
child receipt plus a root bootstrap receipt. A failure rolls back the seed,
including newly inserted global principals; installed schemas may remain.
Matching operator, root ID and deployment-keyed whole-plan fingerprint replay
the original receipt without resetting later publications or key generations.
An existing namespace without that matching root receipt conflicts. Operators
must retain the same identity and private plan after an uncertain reply. Database
provisioning and deployment-key distribution remain external prerequisites.

The operator capability is separate from inference authentication. Namespace
policy, credential, publication, correlation-rotation and execution-resolution
grants are explicit. The existing `api_keys` table is database-wide; principal
reads and mutations require an explicit `global_principal_write` capability,
not merely namespace policy administration. Principal updates lock the same
global row used by admission and preserve its tenant identity. Their opaque CAS
revision covers the authorization fields with a deployment-private keyed MAC;
responses expose neither the verifier nor the plaintext inference key.

Every operator mutation has a caller-owned operation ID. Namespace, operator,
command and serialized input are bound by a private keyed fingerprint. A durable
receipt, audit identity and the mutation commit in the same transaction. A
same-ID, same-input replay requires the current command grant and returns the
original receipt; different input conflicts. Receipt lookup is actor-scoped,
checks the recorded grant, and distinguishes committed from unresolved. Absence
does not establish failure while an earlier request may still be in flight.
Timeout or an unclassified backend error yields an unknown write outcome, never
permission to invent a fresh operation ID and repeat the mutation blindly.

Tenant/resource writes compare policy revisions and preserve occupancy/rate
counters. Credential import compares the slot version and preserves its resource
identity. Publication compares only the configuration generation; correlation
rotation compares only the key generation. The paired revision in a receipt is
the committed observation, not a whole-pair CAS. Publication and rotation acquire
the exclusive head before any receipt/subordinate lock; other mutations acquire
the shared head. Publication authenticates and decodes every referenced stored
credential under that fence. Configuration validation/encoding and input bounds
are checked before the short transaction. No operator command joins a serving
incarnation or executes provider work.

An optional process-local `operator` block enables a separate numeric-loopback
listener with explicit actor IDs, token environment references and grants. The
complete literal block is validated before environment expansion. Tokens are
loaded before database pools open, must contain 32–4096 Bearer-compatible bytes,
and are represented by process-private MACs after setup. No inference key,
legacy control credential or deployment key grants HTTP operator access. Remote
access uses an external TLS proxy or tunnel. Actor identity must remain stable
across replicas and token rotations to recover its own receipts.

The operator listener exposes `/operator/v1/mutations`, `/operations`,
`/revision`, and secret-free tenant/resource/principal/credential/execution
metadata reads. Identity parameters are encoded query values, not URL path
segments. Authorization precedes query/body parsing. Router replies are non-cacheable;
mutation requests require a single complete JSON media type and no content encoding.
Connections are capped at 1–256 (default 16), JSON input at 2 MiB, and the whole
handler at the configured request timeout, including body reads and SQL waits.
The backend's encoded actor-plus-input limit remains an additional acceptance
check. A timed-out write is Unknown, with its validated operation ID when known;
a timed-out read is Unavailable. Synchronous bounded parsing is not preemptible.

`bitrouter_sdk::operator::OperatorClient` takes an explicit HTTPS or loopback
HTTP endpoint and a separate token. It performs one mutation request, without
discovery, retry, redirects or automatic response decompression. Validated definite
4xx rejections are distinct from uncertain delivery, malformed or mismatched
receipts, and other statuses. Uncertain errors retain only the original operation
ID; callers keep the original input privately and use actor-scoped receipt lookup.
Cancellation also leaves an unknown outcome. Lookup absence never proves rollback.
Default response input is bounded to 2 MiB and transport deadlines are finite.
Remote CLI administration remains a separate integration requirement.

Operator serving remains independent of inference readiness during operation.
Initial assembly still fails fast through the common cleanup path. Both listeners
are supervised during startup and running, share the original shutdown deadline,
and join before forced pipeline reclamation and telemetry shutdown.

Local process configuration owns listener addresses, database connection details,
deployment-key references and worker bounds. A versioned, validated shared
snapshot owns provider/account bindings, model/router/policy definitions and
published guard configuration. File-backed policies and policy-lock data must be
included in that snapshot rather than read independently by each replica.
Environment-derived provider secrets are imported into encrypted shared storage;
they are not re-resolved from each worker's environment during inference.

The process database connection budget is 2–128, split into one dedicated
heartbeat connection and the remaining main-pool connections. Generations reuse
these pools. The heartbeat capability derives from the original joined owner,
preserving namespace, deployment key, lease and transaction deadline; it never
joins another incarnation. Renewal validates authority and the owner row, keeps
draining owners non-accepting, and returns database-clock lease evidence only
after commit. Pool isolation cannot bypass authoritative row locks. Expiry is
terminal; a timeout or uncertain commit acknowledgement proves neither renewal
nor expiry. A successful heartbeat alone does not make the main backend or a
loaded snapshot ready.

The official foreground entry selects a literal mode from one bounded (16 MiB)
configuration read before local daemon discovery, directory creation, CWD changes
or local policy/credential discovery. Shared inputs contain only process settings.
Local interpolation retains its existing substitution-first semantics, but its
expanded document must remain local and is reused as the startup baseline.
Environment values cannot switch a selected local launch into shared mode.

One process lifecycle owns the joined incarnation, dedicated heartbeat,
publication, retirement, custody and recovery workers, all generations, optional
HTTP transport and telemetry. Authority polling stays independent of old-generation
retirement, and freshness checks continue while heartbeat SQL waits. Shutdown
withdraws locally before its bounded durable withdrawal attempt, then borrows
all joins under one retained deadline. Cancelled maintenance requeues custodial
entries. Force broadcasts before joins, proves external transport quiescence
before pipeline force, and stops telemetry after producer joins. Startup and
listen failures use the same lifecycle, including the no-transport case.
Terminal outbox receiver integration remains a separate delivery requirement.

Shared snapshot preparation currently rejects `google_extensions: true` on Chat
models. That extension's tool replay proofs require static target credentials;
the shared credential-version binding does not yet provide that replay contract.
Ordinary Chat Completions and local Google extension behavior are unchanged.
This is an explicit shared-mode compatibility limit, never a silent downgrade.

Shared assembly also rejects MCP servers, active server tools, decision-model
execution, request checkers and the agent API before constructing application
resources. These capabilities have no shared execution/worker ownership
contract yet. Local assembly retains its existing behavior.

Configuration publication uses compare-and-set against the current generation.
A replica prepares a complete immutable routing/policy/key snapshot and exposes
its `(config_generation, key_generation)` revision atomically. Neither component
may regress and at least one must advance, including key-only publications.
Every new admission checks both authoritative components; a stale or
unavailable snapshot produces a bounded 503, never a request authorized against
an independent stale policy. Already admitted work retains its recorded routing
snapshot. A configuration-generation change before an attempt's dispatch
authorization rejects that attempt with `authority_changed`; it does not
silently reroute the accepted execution. Correlation-key-only rotation does not
invalidate already accepted provider work. Already dispatched attempts may
finish. Credential/principal authority is checked again at provider dispatch.
Readiness reports the active/loaded generations and backend accessibility.

Snapshot acquisition and registration of request custody share one short
publication gate before body aggregation or admission. Each assembled snapshot
has a distinct process-local resource identity, even when two candidates have
the same authority revision. Swapping the current snapshot prevents later served
requests from registering against the old one. Unstarted, uncertain-ACK and
accepted reservations all retain their generation count until reconciliation
retires them; cancellation never clears that count early. Models/metrics readers
retain their snapshot across awaited work. A retired snapshot with no remaining
custody/readers has reached only the local request barrier: required finalizer,
continuation compensation, owned task and exporter drains remain separate
requirements before releasing its application resources.

The generation manager retains the complete assembled application, not just its
HTTP snapshot: one current generation, at most `max_retired` old generations
(default 4, range 1–64), and one candidate/build/failed-cleanup slot. A full retired
budget prevents publication without evicting any owner. Terminal withdrawal
also closes models/metrics acquisition and cannot be undone by readiness updates.
Build handles and outputs stay in these slots through cancelled waits and failed
publication; diagnostic reports contain no application handles.

Forced shutdown broadcasts build/native cancellation to every retained slot
before waiting. Native force interrupts a graceful database wait without starting
compensation or producer-close SQL. Completed but unobserved builds retain their
complete applications and inherit the permanent force state when joined. Pipeline
force requires actual transport quiescence (or proof that transport never
started), followed by its independent zero-source check. Cancelled join waiters
leave all handles in their owners. Reports distinguish local task quiescence
from unresolved custody, native markers and unconfirmed durable producer closure;
force never manufactures successful reclamation or renews a graceful deadline.

Each candidate retains a fresh producer identity in memory before asynchronous
registration; a confirmed commit makes its record durable. Registration and
absence-close serialize on that identity, using
a closed tombstone to fence delayed creation. Closing an unpublished failed
candidate requires deployment authority even if its continuation key is invalid;
native-row authentication is never bypassed. A closed producer cannot reopen,
and discarding one candidate cannot close another generation's producer.

Normal retirement joins the build and pipeline, confirms native producer closure
and compensation, then checks custody and snapshot readers. It never forces a
worker. Custody reconciliation visits complete fair sweeps in cooperative batches
under the same absolute deadline; a busy first batch cannot starve eligible old
entries. Reclamation is separate from historical panic/finalizer failures.
Retries retain their original attempt deadline; only an explicit new attempt
can choose another deadline. A bounded report history preserves completed results.

Shared pipelines retain separate execution and settlement task sets. Every HTTP
producer acquires a non-clonable source ticket under the snapshot publication
gate. Publication closes the previous source gate; existing tickets survive
through destruction of the request or response body, including a body retained
after EOF. Models/metrics readers also hold tickets. A pipeline cannot be bound
to two serving snapshots.

Drain waiters borrow these task sets; cancellation and elapsed deadlines retain
all handles. Closed sources, zero tickets and both joined task sets establish
the task-producer fixed point. Joining records cancellation, panic and required
finalizer failures separately; a successful task return does not erase a failed
compensation. Returned panic payloads are destroyed outside the ownership lock
and before checking the fixed point, since their destructors can register work.
At that fixed point the task registration gate seals. Late registrations are
owned, discarded without polling their work, and recorded as lifecycle errors.

Forced shutdown first destroys and joins external transport producers. Only
then may it permanently force the pipeline groups. Tasks subsequently registered
by cancellation destructors remain owned and joined, but their work is never
polled. One deadline governs graceful work; forced joining does not create a
new grace period. Noncooperative tasks cannot be reported as a clean shutdown
merely because a waiter timed out. Native continuation reconciliation, custody
retirement and complete application/exporter ownership remain independent
barriers after pipeline tasks have joined.

Shared continuation workers have a separate, irreversible drain gate. Stopping
or forcing the worker never moves its join handle into a temporary waiter.
Cancelled waits retain that handle and the closed restart gate. A worker panic
is joined and recorded; it does not silently start another worker. After the
pipeline producer barrier, graceful continuation drain uses the same absolute
deadline for worker completion, identity locks and every reconciliation batch.
Retrying an expired deadline cannot start another batch. A later, explicitly
selected retirement attempt can use a new deadline on the retained owner.
All pending markers count toward unresolved ownership, including `Reserved`;
drain must not delete such markers just to report success. Forced joining
starts no database compensation and preserves all unresolved markers.

Shared generations borrow one process-owned telemetry exporter, or an explicit
disabled view. Generation assembly never falls back to the local exporter
factory, resolves application telemetry environment/account state again, or
installs another global propagator. A generation's observation status view does
not own shutdown. Process startup remains responsible for resolving its telemetry
configuration and installing global instrumentation once.

The process retains the blocking provider shutdown handle and the actual SDK
processor roots independently. Cancelling or timing out a shutdown waiter loses
neither owner. Graceful shutdown uses the original absolute deadline; an expired
attempt starts no new shutdown. Forced joining aborts and joins the processors
before requesting provider teardown without a new flush, while retaining any
already-running blocking shutdown. Completed outcomes are cached, including
flush/shutdown errors and task failures. Processor quiescence is distinct from a
clean result, historical delivery success, and transport-internal task closure.
The latter remains a separate process-level acceptance obligation.

An incomplete owner Drop must not synchronously wait on an SDK receiver whose
destruction needs the same runtime thread. Processor futures retain the exporter
until their receivers have been destroyed; the runtime adapter references the
owner weakly to avoid a cycle. Dropping an incomplete owner still does not prove
joined shutdown. Constructor validation precedes either processor's startup, so
invalid trace queue bounds cannot unwind through a partially running provider.
The underlying OTLP SDK's standard environment is read during process exporter
construction; this contract does not claim the dependency stack is environment
independent.

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

Shared native continuation storage has its own namespace-scoped relation under
the shared migration ledger. It never initializes or queries the local native
singleton key epoch. The initialized namespace key ring is authoritative;
correlation-only rotation preserves an unchanged continuation key. Both identity
framing and authenticated encryption bind the namespace. Shared ciphertext cannot
be decoded through the local native domain, even with identical key material.
Shared target fingerprints also use versioned tuple framing for provider,
account, protocol, effective endpoint, authentication scheme and credential
proof. Embedded delimiters cannot make distinct field tuples equivalent. Shared
resolution must reuse this framing; the persisted local fingerprint is unchanged.
Native ID bytes are bounded before copying; the complete serialized plaintext,
including its format prefix, is bounded to 1 MiB before database access. Random
publication generation strings are limited to 128 bytes. The 2 MiB persisted
record bound remains a final check after authenticated encryption and encoding.

Each native publication transaction fences key authority, locks its producer
record, takes a transaction-scoped identity lock, and locks an existing native
row before reading the database's current time. The identity lock covers absent
rows as well as updates. Creation, lease and retention times use canonical UTC
microseconds. A final write predicate rechecks lease/retention validity after
sealing, so waiting for a lock or pausing after a clock sample cannot renew an
expired token. Every forward boundary must validate exact ownership afresh;
neither a cached future lease nor a cleanup token authorizes forward publication.
Renewal never extends retention.

Forward publication proceeds from provisional to delivering, then active. Each
boundary independently locks and authenticates the exact generation, producer
and observed lease, checks database time, and commits before the caller resumes
its delivery handshake. Cleanup or unobserved intent cannot advance these
states. Retrying an already completed transition still validates its authority.
An active lookup uses retention, not the publication lease, and returns a
point-in-time authenticated result; it does not grant permission for a later
provider operation to skip its own checks.

Retention expiry replaces the shared payload with authenticated empty plaintext
under the `expired` state. Owner, target, publication and original retention
metadata remain authenticated until physical removal. Empty live payloads and
nonempty expired payloads are invalid. Local payload compatibility and local
NULL-based scrubbing remain unchanged. An expired identity is not rebound until
its tombstone has been purged. Expiry state cannot resurrect when the database
clock moves backward.

Each maintenance pass shares one total database deadline and processes at most
32 rows. Its namespace-scoped cursor advances immediately before each row
attempt and wraps only after exhausting the suffix. Cancellation or a lock
timeout preserves the unattempted suffix for the next pass. Corrupt rows remain
reported and do not starve later identities. Scanning supplies hints only.
Mutation reacquires identity and row locks, authenticates the current
snapshot, and compares its full body and revision before acting. Final SQL
predicates independently recheck database expiry or purge time. This prevents
a stale scan from deleting a replacement even when both rows have revision
zero. Existing tombstones are excluded from expiry scans until physical purge
is due.

The producer record is a durable bind gate. Bind transactions hold its shared
lock through commit; closing takes an exclusive lock and commits irreversibly.
After the actual pipeline producers/tasks join, the generation owner closes this
gate before resolving uncertain `Reserved` markers from row absence. An
unconfirmed close is not a barrier: a fresh primary read must confirm the exact
producer's closed record. Evidence lookup after closure uses a fresh statement
snapshot after acquiring identity locks, including commits by other producers.
Closure proves only that this producer cannot insert later; worker joining,
marker compensation and complete generation resource ownership remain separate
requirements. A closed producer identity is never reused or recreated.

Shared registry construction explicitly requires the initialized continuation
authority. It routes binding, forward publication, lookup, retention and
reconciliation to the namespace backend; compiling the feature does not change
a local registry's storage mode. Assembly derives its continuation key and
database from the same capability rather than accepting independently supplied
values. The local constructor and local persistence remain unchanged.

Shared pending records distinguish an uncertain reservation, a confirmed owned
binding and a non-owning preparation against an established active binding.
Every record retains the original random generation and invocation receipt.
An observed lease is invalidated before an awaited renewal; only a fresh locked
observation of a confirmed live owned generation can reconstruct it. A receipt
alone never reconstructs forward authority from an uncertain reservation.
Retiring or uncertain state cannot revert to forward publication.

Explicit invocation-matched rollback records its obligation before waiting for
an identity lock. If reconciliation still retains that invocation, rollback
returns an unresolved error instead of acknowledging cleanup. Positive
cancellation may atomically demote its own active
generation to authenticated `cleanup` state. Uncertain ownership without
positive failure can claim only expired prepublication or existing cleanup.
Producer closure can claim prepublication but cannot demote retained active
state. Cleanup preserves the generation and retention metadata and never grants
a publication lease. A peer can finish a durable claim, comparing its full body
and revision; cancellation after claim commit does not strand an active row.

The delivery sender retains a monotonic process-local outcome independently of
the terminal receiver. Success requires successful accounting and terminal send
in the same final poll; failed delivery may also send the terminal `()` signal
and must not become success evidence. Cancelled finalizer waits cannot consume
the retained outcome. A successful terminal permits local intent retirement
without deleting its active mapping. An active row with unknown delivery remains
retained, even after the worker and producer close. Non-owning preparations never
delete the established binding. An eligible commit with no preparation is an
error, not an implicit duplicate success.

The owned drain closes its in-memory bind gate, joins the worker, and then closes
the database producer, even when the marker map is empty. It checks the original
deadline before starting producer-close SQL. Reserved missing/foreign evidence
is accepted only after confirmed closure. Forced drain only joins workers and
retains unresolved records; it starts no compensation.

Official startup, whole-generation resource ownership and independent process
acceptance remain staged. These registry and database fixtures do not close a
P1 independent-process acceptance gate.

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
or execution-custody lifetime. A retained transport owner holds the accept task
and a separate connection task set. Accept cancellation or panic cannot destroy
the connection owner. Registration and force use one gate, so a previously
accepted socket cannot register a task after shutdown closes that gate.

Graceful stop selects the process's absolute deadline, releases the listener and
notifies existing connections. The owner joins the accept task before taking
over connection reaping. Expiry or cancellation retains every unobserved join;
retrying cannot extend the deadline. Force signals all retained transport roots
before awaiting any, permanently prevents further graceful attempts and requires
actual joins. Reports distinguish quiescence from forced, cancelled or panicked
history and include returned panic-payload destruction in the join barrier.
Application-spawned inference/finalization tasks still require their separate
custody and task owners; transport quiescence alone does not establish a clean
process shutdown.

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
