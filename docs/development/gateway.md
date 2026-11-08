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

## Provider scheduling boundary

Status: **local provider scheduling implemented, explicitly enabled**.
Gateway-only assembly omits the semantic policy runtime, selector and decision
backend. Semantic builds retain model decisions. Scheduling starts after route
hooks and frozen request checks; disabled requests retain the existing chain and
allocate no queue, reservation or prefix state. Configuration belongs to the
[canonical configuration contract](../specs/configuration/README.md#inference-routing-configuration).
The runtime uses application modules, with no new crate, RPC or public plugin ABI.

### Composition and feature ownership

Keep `ModelSelector::prepare_selection/select_variant` as the semantic seam.
`PolicyRuntime`, policy selection and the TypeSafe decision transport compile behind
`semantic-routing`. Static resolution, request checks, continuation validation,
protocol conversion, execution and settlement stay in the gateway.
Pure input validation does not require either routing feature.

| Build | Model decision | Provider scheduling |
| --- | --- | --- |
| No default features | Explicit model/static mapping | Existing static chain |
| semantic-routing | Existing policy selector | Existing static chain |
| inference-routing | Explicit model/static mapping | Local scheduling when enabled |
| Both routing features | Semantic decision first | Local scheduling when enabled |

Neither routing feature enables `core`, `harness`, MCP or an exporter.
Full/default builds include semantic-routing for compatibility and compile
inference-routing with activation default-off. Existing gateway release archives
retain semantic capability by explicitly selecting semantic-routing; the truly
minimal no-default-feature build is tested separately as a custom profile.
Build identities, transition checks and distribution manifests use these same
feature sets.
A feature-off build rejects configured semantic policies and decision-model
activation, rather than ignoring them. Static router/check bindings remain valid.

The existing semantic assessment uses the assembly-owned single-attempt typed
decision executor, separate from generation and its model selector. It cannot
re-enter selection through a generation alias. The following auxiliary generation
contract applies when that path is introduced: an assembly-created pipeline with
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

The implemented reservation foundation is `gateway::scheduling`. Assembly owns
one runtime owner; tenant service clones share its write-once attachment and
per-execution pipelines borrow it without closing the pool on completion. The
owner reads the routing table's published configuration/generation at entry.
Each enabled request keeps an immutable Scheduler snapshot (policy, DRR weights,
absolute time budget and prefix projector). One lazily allocated pool/worker
survives disable/re-enable, including active and uncertain work. Disabled startup
creates only the owner, with no pool or worker. The existing reload coordinator
validates the complete candidate before routing-table publication; mixed cold/hot
changes fail before publication. No new reload wire participant is introduced.
Old queued entries retain their own weight/policy/TTL. Prefix namespaces separate
generations and evidence retains its original TTL. Shutdown closes the shared
pool and joins its sole worker. `drain_pending_settlements` remains a reusable
settlement barrier. Required local/owned drains terminate an allocated pool;
before first activation the legacy absent-scheduler drain remains reusable.
A previously enabled pool is still terminal when currently disabled. Explicit
process-owner shutdown always closes admission.

For a scheduled attempt, the frozen ordinal, process generation and exact target
allocation identify the candidate; destination tuple and protocol are checked again
at booking and send. This handle is not credential or execution authority. The
pipeline owns the non-cloneable reservation before `reached_hop`; a successful
stream handoff transfers it to the returned producer. Only the concrete HTTP
send and protocol collectors emit mandatory evidence. Non-2xx rejection follows
the existing receipt status rule; successful headers retain capacity until body
and terminal/EOF validation. Ambiguous errors stop fallback even where disabled
ordinary execution retains a more permissive legacy policy.

Every active booking reserves an uncertainty credit. Drop before send rolls back;
drop or timeout after send retains work until explicit terminal evidence or TTL.
The owned reaper and synchronous admission cleanup bound uncertainty and sample
retention; expiry never changes a durable receipt or creates retry permission.
Shutdown closes booking/send admission and joins the reaper, reporting remaining
active custody rather than claiming provider termination. Inference settings
remain restart-required, so reload cannot reconstruct or reset live counters.
The reaper enforces deadlines independently of observer awaits and stream polls.
Expired unsent owners cannot resume a send; expired sent owners become uncertain.
Ownership entries and bytes remain charged until the producer actually drops,
even after advisory uncertainty expires. Scheduled HTTP uses the existing
no-redirect/no-retry client, and validates terminal ordering before exposing
stream parts. Only explicit gateway fallback can start another reserved attempt.

Read-only inspection exposes local counts, outstanding estimated work, typed
outcome counters and success-conditioned timing summaries. It never books,
expires, samples or warms state. Measurements retain at most 32 samples per
destination, separated by input/output bucket and response mode, for five minutes;
three comparable samples are required for a mean. JSON duration is not TTFT.
The current work estimate counts canonical serialized prompt bytes without
retaining an encoded copy. Adapter prefix projections use independent framing
and do not change that frozen work charge.
State charges destination strings, fixed attempt records and bounded samples
against configured logical entry/byte limits before retention. These limits are
not a prediction of allocator RSS or upstream physical capacity.

The local fair queue enforces global and per-tenant request/retained-byte limits
before storing a waiter. One owned worker dispatches pinned FIFO destination lanes using
weighted tenant deficit, with a persistent cursor and a 64-entry tick. It also
owns reservation expiry; a fully capacity-blocked pass sleeps on actual changes
or the earliest deadline. Cancellation removes the waiter, and an abandoned grant
releases its reservation. Closing drains waiters with the existing unavailable
error before joining the worker. Read-only trace inspection exposes bounded totals
for admission, dispatch, full, expiry, cancellation, closure and state refusal,
plus current count, bytes and capacity/credit waits, without tenant labels.
Adaptive first attempts have individually charged lanes, so a pinned request
cannot trap work eligible for another destination. All lanes within a tenant
spend the same deficit and preserve the older dispatchable head's earning turn.

Tenant identity comes from admitted TenantExecution or authenticated caller
identity. Anonymous/local work shares one local flow. The existing configuration
defines tenant weights and a single default service class; request headers and
`service_tier` cannot select priority. No provider RPM or health gate runs while
waiting. After a grant the existing live authority, rate and receipt gates run.

Queue bytes cover request-owned prompt/context storage, retained target copies,
host DTO events and extensions, including spare String/Vec capacity. Opaque JSON
map storage and inbound header backing are compacted before retention. Shared
deployment and authority graphs stay with their existing owners and budgets.
Private host producers explicitly account skipped/opaque state; unaccounted
plugin events/extensions are rejected before queue admission. This is a logical
allocation bound, not an allocator RSS guarantee. Disabled assembly does not
allocate the private accounting map or callbacks. Existing native/continuation
history and shared scheduled roots remain unsupported.

The following signatures describe ownership contracts. Concrete internal names
may differ; the operator embedding API is `gateway::planning::rank_manifest`.

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

### Local approximate prefix evidence

The prefix-affinity runtime now owns a private, process-keyed index. Each built-in
adapter owns a versioned semantic projection: typed generation controls and tool
prelude, then system instructions and ordered message/content boundaries. Frames
are UTF-8 with explicit lengths and typed delimiters; this is not a serialization
of HTTP or a claim about engine tokenization. Stream/store/request identifiers and
credentials are excluded from content. Rendering compatibility, model, provider,
endpoint/account, configuration generation and credential scope partition the
cache domain. Tenant execution uses its retained credential resource revision;
static transport uses a blinded credential commitment, never a raw key identifier.
Existing preflight still rejects dynamic auth/body appliers before scheduling.

Projection hashes at most max_projection_bytes in complete 256-byte blocks using
chained HMAC-SHA256. Discarded string tails contribute their byte lengths without
being scanned/hashed; a structural visit cap equal to the projection limit bounds
large collections. The full framed byte length remains the match denominator.
No raw prompt, credential, usage object or exportable digest is stored in the index.
Only private request/attempt metadata holds bounded digest vectors. Unknown raw
provider extras, unproven metadata, multimodal/opaque content, dynamic header
cache domains or exhausted projection work disable affinity without altering a
valid request. Text, function tool schemas and typed ephemeral cache breakpoints
are supported conservatively; changes to these projections require adapter
version changes. Cross-tenant cache sharing is absent; anonymous callers receive no affinity.

Each block contribution has independent attempt ownership. Booking records short
(at most one second) tentative placement; send records recent exposure; validated
success records successful exposure. Rejection/unsent cleanup removes only that
attempt. Unknown outcomes retain diagnostic exposure but score zero. Cached read/
write counts require normalized provider-reported evidence; missing cache fields
remain unknown, explicit numeric zero is retained, estimates are ignored. Cached
usage never asserts residency of a particular block or adds affinity credit.

TTL/decay and an LRU of observation updates bound index belief and memory separately
from reservation custody. Read-only lookup neither refreshes LRU nor inserts or
expires data. Only contiguous complete-block matches count; current-attempt
exclusion is available and pre-booking inspection cannot self-warm. Per-entry
bounds include all three ordered indexes; live attempt digest storage is charged
to the existing state budget. Index pressure may discard advisory evidence or
omit tracking, and never frees capacity, changes receipts or grants replay.
The queue's owned worker also reaps prefix deadlines. Default load-only scheduling
creates no prefix projector/index. Internal provider policy consumes only the
read-only matching path, without scanning diagnostic counters or updating evidence.
Public activation uses the same bounded queue, evidence and authority path.

### Provider planning implementation

The queue worker refreshes destination pressure, fresh shape-qualified latency
and optional prefix evidence immediately before the first ordinary booking.
Ranking and booking share the scheduler mutex; no provider, authority or health
owner is called under that lock. The frozen score above uses checked u128 rational
cross-products; common denominators cancel without rounding pressure or score
ties. Affinity is already rounded down to millionths by the prefix index.

Rank the bookable candidates repeatedly under the 110% pressure band, choosing
minimum discounted score and applying latency only when every exact-score tie
has at least three fresh matching samples. Otherwise choose original ordinal.
Unavailable candidates follow in original ordinal order. This full sequence is
frozen before the first per-hop execution admission. Every later attempt queues
for its next original target and never reranks. Current live admission may still
refuse it; the existing fallback classifier and unknown-outcome fences decide
whether another attempt is legal. Target storage and model/prompt are unchanged.
Ranking is bounded by the frozen membership cap of 4096; temporary ranking memory
is finite, and retained order storage is charged before queue admission.
The gateway records an accounted compatibility mask using pure executor input
preflight, without eagerly invoking conversion observers or authority rejection.
First adaptive ranking excludes known incompatible inputs and appends their
original ordinals after the eligible sequence. Reached exclusions still run the
existing lazy callbacks/aggregation without queueing. All-excluded input bypasses
the queue entirely. Pinned/ordered and frozen fallback candidates run existing
per-hop conversion/native-control admission before any queue wait or DRR charge.

An adaptive first waiter can inspect any destination in its frozen set, while
subsequent attempts and caller-supplied ordered requests remain pinned. Shared
pressure includes reservations, sends and uncertainty, so the next queued request
sees the previous grant before it scores. Foreign instance/generation handles
are refused. No booking race spins or independent health circuit is introduced.

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
planning authority. Operator-host integration tests consume the same application-owned
ranking function as the queue. Trusted embedding calls
`gateway::planning::rank_manifest` with existing capabilities and manifest, an
explicit compatible subset represented by original-ordinal `Signal` values,
one fixed attempt work charge and the policy. The host owns model compatibility
and comparable bucket/mode telemetry; the helper does not infer them from opaque
model names. Missing latency remains None. The helper validates the manifest,
negotiated attempt ceiling, installation/authority binding and supplied ordinals,
then returns the existing explicit v2 binding. It neither truncates a selection
nor supplies a generation-defaults approval; callers needing defaults use the
existing explicit reviewed binding workflow. No ranking code moves into the SDK,
and no runtime observation is an execution grant.

Ordinary nonordered tenant candidates can be ranked only inside their retained
authorized set. Profile-protected routes still require explicit ordered mode.
Existing v1/v2 messages, private signatures and receipt formats are unchanged.

### Acceptance contract

Maintain tests for all four reduced-feature compositions, disabled
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

### Local composition verification

`routing_composition` covers static/semantic selection with JSON and SSE, explicit
scheduling, missing-feature rejection, hot reload and cold-change refusal. Its
Cargo-built `routing_topology_fixture` launches two isolated production gateway
assemblies and an HTTP provider in separate owned processes. Inference builds
hold one provider stream while selecting a different provider; semantic builds
choose the model on the user side. The test also checks generation constraints,
shrinking deadlines, stream shutdown, bounded attempts and provider-wire privacy.

Run the same suite in every reduced build (no Core or Harness needed):

```sh
for features in '' semantic-routing inference-routing semantic-routing,inference-routing; do
  cargo test -p bitrouter --no-default-features --features "$features" --test routing_composition
done
```

The test builds its matching Cargo fixture if none is prebuilt. CI can build the
example first and set `BITROUTER_TEST_EXAMPLES_DIR` and
`BITROUTER_TEST_PREBUILT_ONLY=1`; the fixture's reported capabilities must match
the test binary. Never reuse an all-feature fixture for a reduced-feature test.
Children are isolated from ambient credentials, and killed and reaped on failure.
This establishes local synthetic behavior, not provider savings or production
performance. Effectiveness and release performance budgets above require their
separate workload measurements.
