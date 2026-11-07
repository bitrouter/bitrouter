# Gateway adaptation

This development guide follows the current implementation. Update it with
changes to ownership, adapters and runtime behavior. Format compatibility is
maintained in the [specification index](../specs/README.md).

## Endpoints and upstream contracts

| Method/path | Inbound protocol |
| --- | --- |
| POST `/v1/chat/completions` | [Chat Completions](https://platform.openai.com/docs/api-reference/chat) |
| POST `/v1/messages` | [Messages](https://platform.claude.com/docs/en/api/messages) |
| POST `/v1/responses` | [Responses](https://platform.openai.com/docs/api-reference/responses) |
| POST `/v1/decisions` | Decisions, using the [public DTOs](../../crates/bitrouter-ai/src/decisions.rs) |
| GET `/v1/models` | Available only when model listing is enabled |
| GET `/health`, `/metrics` | Host health and metrics; not inference protocols |

MCP endpoints are described in the [MCP gateway profile](gateway.md#mcp-adaptation).
Provider/model configuration and auth determine which operation can actually
run. A listed model or catalog record is not proof of adapter availability,
authorization, pricing or support for every operation. Requests requiring an
unsupported conversion or operation must fail rather than silently claim faithful
execution. The current set of accepted fields, extension handling and failure
behavior is described by the relevant public DTO and adapter; callers should
consult the corresponding vendor specification for the base protocol.

## Streaming and termination

Streaming uses server-sent events where supported. Chat Completions ends with
`[DONE]`; Messages uses `message_stop`; Responses has a completed, incomplete or
failed terminal event. Decisions is JSON. A finish reason, successful HTTP
status, EOF, keepalive or a decoded ordinary event does not by itself prove a
successful protocol end. An upstream/transport error after HTTP headers cannot
change the already committed HTTP status. An implementation must not present a
truncated or mismatched stream as successful completion, and must not synthesize
a successful terminal from a transport error. The application may send its Chat
error-then-DONE trailer; that trailer is not proof of success.

Same-protocol transport preserves supported extension fields and absent/null
distinctions where represented by the public wire types. Cross-protocol
conversions can be lossy or unsupported; client applications requiring exact
fidelity should use a compatible native protocol and verify required features.
A client must tolerate unknown response fields, but must not interpret an
unknown terminal event as success. The SDK does not automatically replay a request after uncertain delivery;
the gateway may apply configured upstream retry or fallback policy. No
provider-side exactly-once execution is promised, and a disconnect may leave
the upstream outcome unknown.

## Shared gateway profile

Shared mode is explicitly enabled and uses independently published routing and
credential authority. It does not inherit local provider configuration. Its
inference transport and service readiness, authentication, resource bounds,
HTTP/1 and optional HTTP/2 behavior are described in the
[shared gateway operator guide](../../skills/bitrouter/references/shared-gateway.md).
A lost owner lease or connection is not evidence that an upstream invocation
stopped. A client must treat uncertain work as unknown rather than retrying it
as a fresh execution. The independently authenticated administration transport
is the [shared operator API](../specs/service-api/README.md#shared-operator-api). The
[shared request identity profile](../specs/service-api/README.md#shared-request-identity) specifies headers,
canonicalization, recovery and a compatibility vector.

## Owned tenant service

The opt-in [tenant service](tenant-service.md) uses an exclusively owned local
SQLite database and a protected control listener. It supports immutable resource
publication, revision-bound SDK dispatch and durable execution receipts with
separate native usage evidence. Its ownership and recovery model is distinct
from shared gateway mode; the two profiles cannot be enabled together.
[Tenant routing](tenant-routing.md) describes the actual-send limits and health
state inside the authorized retained candidate set.

## Adapter validation

Authentication, routes and model availability are deployment-specific. Unknown
fields and validation are governed by each protocol and its selected adapter;
this adapter guide does not promise that an arbitrary unknown request field is
forwarded. The [adapter contracts](../../crates/bitrouter-ai/src/protocol)
and their consuming tests define the supported conversion shapes.

## MCP adaptation

When configured, the gateway accepts JSON-RPC requests at `POST /mcp/{server}`
and an aggregate path (normally `/mcp`, configurable). Direct requests select
one named upstream; the aggregate merges eligible upstreams and namespaces
items. The gateway is not an arbitrary JSON-RPC tunnel: it relays supported
standard methods and an explicit extension allowlist. Unsupported methods fail
rather than being forwarded. Downstream connection identity and request context
are not forwarded as the upstream's own identity.

When `Accept: text/event-stream` is requested, gateway frames contain JSON-RPC
messages in SSE `data:` events. The stream ends after its final result or first
error. Without streaming, the response is JSON. Protocol failures use JSON-RPC
error objects, and HTTP status can also reflect a transport or pipeline failure.
A client must handle both layers and must not infer execution success solely
from an HTTP status.

## Negotiation and skills

The upstream configuration `mcp.upstream_protocol` selects lifecycle:
`auto` (default) and `2026-07-28` use modern discovery with classified legacy
fallback; `latest` selects the legacy initialize path. Discovery timeout or a
correlated legacy-classified JSON-RPC error can trigger fallback. Transport,
malformed or uncorrelated replies and modern security/metadata rejections do
not authorize fallback. Upstream tasks or input-required tool results that the
gateway cannot represent are errors, not synthesized successful tool output.

The supported skills extension methods are `skills/list`, `skills/get` and,
when advertised, `resources/directory/read`. On aggregate routes `skill://`
URIs are namespaced by upstream and reversed on dispatch. Unsupported URI
schemes are not silently converted into a different upstream's identity.
Unrecognized extension metadata is preserved where represented by the skills
wire types. Source permission and per-skill consent are independent of the
existence of cached content; listing a skill does not authorize execution.
Upstream connections use configured server credentials, not a downstream user's
inference credential. Connection sharing does not confer tenant isolation.

Schemas and bounded skills representations are in the [wire types](../../crates/bitrouter-ai/src/protocol/mcp_skills.rs).
The protocol is evolving; callers must use discovered capabilities, tolerate
unknown optional response fields, reject unsupported required behavior and
handle errors.

## MCP connection ownership

The gateway retains configured upstream clients, aggregation, server-side tool
execution and skill relay; it does not ship a separate first-party local
control-origin MCP server. Shell-capable local agents use structured CLI and the
BitRouter skill. The upstream connection pool is keyed by configured server
name and uses daemon-scoped credentials. Deriving per-request headers alone
cannot isolate callers sharing that connection: a future per-caller credential
model would need an appropriate pool identity and authorization design.

A native harness uses its own explicit source grants and execution ownership.
Gateway cache membership, upstream metadata and downloaded skill bytes do not
confer native consent or workspace authority. Keep those boundaries with
[harness resource loading](harness-resources.md).

## Proposed provider scheduling boundary

Status: **proposed, unimplemented, pending review**. This is the concrete module
contract for the provider inference router; existing behavior above remains the
reference until implementation. Configuration belongs solely to the
[canonical configuration proposal](../specs/configuration/README.md#proposed-inference-routing-configuration).
The runtime uses application modules, with no new crate, RPC or public plugin ABI.

### Composition and feature ownership

Keep `ModelSelector::prepare_selection/select_variant` as the semantic seam.
`PolicyRuntime` and semantic assessment/decision-model execution move behind
`semantic-routing`. Static resolution, request checks, continuation validation,
protocol conversion, execution and settlement stay in the gateway.
Pure input validation does not require either routing feature.

| Build | Model decision | Provider scheduling |
| --- | --- | --- |
| No default features | Explicit model/static mapping | Existing static chain |
| semantic-routing | Existing policy selector | Existing static chain |
| inference-routing | Explicit model/static mapping | Opt-in scheduler |
| Both routing features | Semantic decision first | Opt-in scheduler for that decision |

Neither routing feature enables `core`, `harness`, MCP or an exporter.
Full/default builds include semantic-routing for compatibility and compile
inference-routing with activation default-off. Existing gateway release archives
retain semantic capability by explicitly selecting semantic-routing when their
manifest is changed; the truly minimal no-default-feature build is separately
tested as a custom profile. That artifact selection change must be reflected in
build identities, transition checks and distribution manifests together.
A feature-off build rejects configured semantic policies and decision-model
activation, rather than ignoring them. Static router/check bindings remain valid.

Semantic auxiliary inference uses an assembly-created dedicated pipeline with
concrete model, no model selector and no decision runtime. Its caller, cost
budget, cancellation and deadline are inherited; existing paid-work gates remain.
An internal operation tag forbids recursion. It may share provider scheduling
state when enabled, but selection finishes before booking a generation slot,
avoiding a parent-held-slot deadlock.

Two gateways preserve the selected model and protocol constraints. The upstream
user-side gateway treats a transport-ambiguous downstream result as unknown when
scheduling is enabled; it does not start a second downstream execution. Plain
protocol forwarding cannot promise transitive exactly-once execution or transmit
an authenticated tenant identity implicitly. Each gateway derives its own caller
from its own authenticated policy.

### Minimal internal interfaces and consumers

The following signatures describe implementation contracts, not currently
exported Rust APIs. Add types only with the consuming implementation.

| Interface | Input / output | Consumer |
| --- | --- | --- |
| Existing ModelSelector | Policy snapshot plus context -> explicit model and allowed context | Pipeline::resolve_route |
| Scheduler::enqueue | Owned AdmissionRequest -> cancelable QueueTicket | General pipeline and TenantExecution::pipeline |
| Scheduler::acquire | QueueTicket -> owned Reservation or bounded refusal | Generation fallback loop, before reached_hop |
| Reservation::begin_send | CandidateId + attempt identity -> Sending state | HttpExecutor JSON and stream send gates |
| Reservation::observe | Typed AttemptEvent -> idempotent state update | Concrete executor and protocol collector |
| Reservation::finish | Typed AttemptOutcome -> terminal cleanup | Attempt owner, including stream Drop |
| Planner::rank_manifest | Authorized manifest, bounded advisory snapshot -> ordinal list | Trusted operator host composing existing SDK manifest and ordered dispatch APIs |

`AdmissionRequest` owns a request handle, authenticated flow ID, fixed work
charge, monotonic absolute deadline, immutable policy generation and opaque
candidate IDs. The pipeline retains targets/credentials/prompt; the scheduler
cannot retrieve secrets, change a prompt/model or add membership.
`CandidateId` binds the generation, original ordinal and exact execution identity.
`Reservation` is non-cloneable; observations use a bounded attempt ID, not
ownership clones. These application-private types are not serialized.

One scheduler instance is owned by assembly. Tenant per-execution pipelines
receive its restricted handle; creating a pipeline must not create another
scheduler. Disabled assembly uses the existing immediate path and creates no
scheduler workers/index. Required cleanup is owned by the executor/task tracker;
best-effort ObserveHook/OTel callbacks only receive diagnostic copies.

### Admission order, lifetime and errors

Prepare model/context and run entry checks first. Resolve/filter compatible
candidates, complete route hooks and freeze membership/prompt, then enqueue.
Protocol preflight remains mandatory for every actual candidate. Queue admission
does not charge upstream RPM or create an armed provider attempt.

Book scheduler capacity atomically under its own mutex, then release that mutex.
The first ordinary acquire ranks the frozen membership and freezes its full
attempt sequence before booking. Fallback considers only the remaining sequence;
it may wait for the next permissible candidate but cannot rerank that sequence.
Caller-supplied ordered requests use their admitted sequence from the outset.
Run reached_hop, request preparation/authentication and the existing final
authority/receipt arm gates in their existing order. No scheduler lock nests
with authority, routing, operational or journal locks; collect an outcome after
those guards are released. Failed unsent admission rolls back scheduler capacity.
Legacy refundable RPM and profiled nonrefundable reached-hop timestamps retain
their different semantics. Authority revocation never waits for the scheduler.

States are Queued -> Reserved -> Sending -> TerminalKnown or OutcomeUnknown.
`begin_send` runs immediately before the transport future is polled, after all
send gates; cancellation in that narrow window conservatively means unknown.
First useful output is an observation within Sending, not a capacity release.
Events are `ResponseHeaders(status)`, `FirstUsefulOutput(elapsed)`,
`ProviderUsage(evidence)`; outcomes are `Unsent`, `Rejected(status)`,
`Completed`, `ProviderFailedTerminal`, `Unknown(reason)`.
Only gateway-validated evidence can construct outcomes.

Drop before Sending removes tentative evidence and releases once. Drop/timeout
after Sending retains the reservation's work charge as uncertainty; it is included
in global/destination capacity and bounded by max_uncertain. Existing active
reservations reserve room for becoming unknown, so conversion never allocates
unbounded overflow. Expiry releases advisory charge once, not receipt obligations
or proof of provider termination. Booking requires both an active slot and an
uncertainty credit; max_uncertain below max_active therefore lowers usable
concurrency. Credits transfer into unknown records or return on known completion.
Uncertainty expiry never creates retry permission. Later terminal evidence cannot release the same charge
again. Shutdown joins scheduler workers and executor owners via existing trackers.

The deadline is min(inherited deadline, ingress time + request_timeout_ms);
queue wait also ends at min(deadline, enqueue time + wait_timeout_ms).
Retries never renew either budget. Enqueue/queue timeout uses existing local 429
mapping; elapsed total deadline uses the existing timeout response; unavailable
scheduler/state uses 503. None creates upstream provenance. No new public error
envelope or header is introduced. Local rate refusals keep their existing mapping.

With enabled scheduling, timeout, malformed/truncated successful body, missing
adapter proof, cancellation and unknown effects forbid fallback. Validated
completion also forbids fallback even when downstream delivery fails. Only a
definitive rejection or proven unsent outcome may reach existing fallback policy,
which still may refuse. Disabled ordinary routing retains its legacy policy.
Streaming never restarts after stream creation.

### Fair queue and frozen score

Weighted deficit round robin runs across authenticated tenant flows, FIFO per
dispatch lane. Weight defaults to one; quantum is 1024 work units per round.
Charge = clamp(ceil(projected_input_bytes / 4) + output_budget, 256, 131072).
Use a validated output limit capped at 32768, or 4096 if absent. Multimodal input
uses bounded canonical serialized byte length; work units are estimates, not
billable tokens. Charge stays fixed, including across fallback, and never receives
a speculative cache discount. Each new dispatch attempt consumes that charge;
a terminal request leaves the flow. Bound retained deficit to 131072 units.

Inspect at most 64 queued entries per dispatch tick using a persistent rotating
cursor. Unavailable lanes keep their FIFO and accrue capped deficit. After each
tick yield to the runtime; an unfinished scan schedules a continuation, while a
fully blocked pass waits for capacity/deadline/cancellation changes. At equal
weight, a maximum-charge head earns sufficient deficit within 128 rounds.
Eligibility and ongoing arrivals must not reset an older head's cursor/deficit.
These progress properties require deterministic tests, not wall-clock sleep claims.

Only candidates with currently bookable local count capacity enter scoring.
Existing rate/health observations are advisory exclusions when fresh; the actual
gate remains authoritative. For candidate d, Wd is outstanding reserved, sent and
uncertain work, w is the new attempt charge, Cd is configured destination capacity:

`P(d) = (Wd + w) / (4096 * Cd)`.

Restrict consideration to `P(d) <= 1.10 * min(P)`. This hard band prevents
affinity or latency preference from overriding materially lower assigned load.
For prefix policy, A is contiguous matched complete-block fraction times freshness
times confidence. Freshness is max(0, 1 - age / TTL); confidence is 1/4 for recent
send exposure, 3/4 for validated successful exposure, zero for tentative/unknown
outcomes. Provider cached usage is a measurement, not evidence of exact residency.
`S(d) = P(d) * (1 - A / 10)`; load-aware mode sets A=0.
Use fixed-point millionths with checked u128 intermediates and cross-products;
round affinity down. No floating NaN, random tie or division by zero is possible.

Choose minimum S. For exact score ties only, use mean observed TTFT (stream) or
completed duration (JSON) when every tied candidate has at least three fresh
samples in the same bounded size bucket; otherwise retain original ordinal.
Keep at most 32 samples per destination/shape, with five-minute expiry. Input
buckets are <=4 KiB, <=32 KiB, <=256 KiB and larger; output budgets are <=1024,
<=8192 and larger. Record failure/timeout counters separately and never label
success-only latency as unconditional performance. Reuse existing health owners.

Frozen policy examples use w=4096 and capacity eight at each destination:

| Outstanding work A / B | Affinity A / B | Expected result |
| --- | --- | --- |
| 0 / 0 | 0 / 0 | Both P=S=0.125; retained ordinal wins without comparable latency |
| 0 / 0 | 0.75 / 0 | A S=0.115625, B S=0.125; choose A |
| 4096 / 0 | 0.75 / 0 | A P=0.25 exceeds B's 0.1375 band ceiling; choose B |
| 0 / 0 | expired / 0 | Expired affinity is zero; retain ordinal |

First useful output includes nonempty visible or reasoning text and the first
nonempty tool-call name/argument delta. Headers, keepalives, role-only frames,
usage and opaque encrypted reasoning alone do not qualify. Record the first
such event once using a monotonic clock; JSON requests have no TTFT sample.

Prefix v1 uses 256-byte blocks, chained keyed cryptographic hashes, ephemeral
process key and tenant/credential isolation. Cap projection work using the schema
limit, retain only a complete safe leading prefix, and divide matched bytes by
the complete projection length (so truncation cannot claim a full hit).
Unsupported opaque/multimodal shapes disable affinity unless an adapter proves
a safe leading prefix. Version the projection; it must include ordered
system/tools/messages and all model-input-affecting controls. No digest export.
TTL bounds belief; LRU bounds index memory. Tentative scoring stays disabled
for this version; reservations still add load immediately. No shared-prefill claim.

### Supported roots and hosted planning

Initial scheduled roots are static Chat Completions, Responses and Messages,
JSON and SSE, for one fixed selected model. Model changes require a new semantic
decision before scheduling. Native Harness/Core roots, Decisions, counting,
embeddings, provider continuation, OAuth refresh/continuation and server-tool
loops remain on existing behavior when scheduling is disabled; an explicitly
scheduled unsupported root rejects before any paid work. Semantic auxiliary
generation is supported only through the dedicated concrete static route above.
A request cannot use an unsupported custom executor to silently bypass scheduling.

Hosted adaptive planning uses a trusted operator host: fetch existing ordered
capabilities and candidate manifest with operator authority, rank before
submission, then submit an explicit selection using existing SDK ordered v2.
Planner input retains all revision/instance/ordinal bindings and only includes
candidates for the fixed compatible model. Read-only ranking books no capacity,
does not warm the prefix index and promises no future availability.
The serving scheduler enforces local fairness/concurrency on that fixed sequence.
It never reranks accepted input. The official server exposes no new planner RPC
and does not automatically convert ordinary inference credentials into operator
planning authority. Operator-host integration tests are the first composition
consumer of the shared ranking function; trusted embedding can use that same
application-owned function without moving it into the SDK.

Ordinary nonordered tenant candidates can be ranked only inside their retained
authorized set. Profile-protected routes still require explicit ordered mode.
Existing v1/v2 messages, private signatures and receipt formats are unchanged.

### Acceptance contract

Before activation, test all four reduced-feature compositions, disabled
compatibility, semantic recursion bypass, both assembly paths, order preservation,
authority revocation during queue/reservation, legacy/profiled RPM differences,
race-limited bookings, idempotent terminal/drop/late-terminal cleanup, bounded
queue/index/state growth, cold/stale observations, prefix isolation, head-of-line
progress, generation reload and shutdown joins. Extend existing Rust fixtures.

Freeze performance budgets at <=5 ms p95 scheduling CPU/wall work for 64
candidates and a 32 KiB prompt in a release build on recorded hardware, and
<=5% p95 end-to-end regression versus load-aware routing on an equal offered-load
no-cache fixture. Report successes, refusals, failures and workload variability;
lower throughput cannot manufacture a latency improvement. Warm-cache results
must compare load-aware and session-stickiness baselines. These are acceptance
targets, not measured properties.
