# Authenticated local passive capture profile

This profile provides a local Rust embedding entry through
`harness::acp::controlled::ControlledSession::open_authenticated`. Dispatch and
strict receipts follow the supported opt-in, single-instance Unix SQLite
[Service eligibility](RESOURCE_TENANCY_IMPLEMENTATION.md#current-local-dispatch-eligibility).
Earlier slice qualification statements below retain their original scope.
Source-pinned local checks do not establish full M2, final aggregate, platform or
performance acceptance; see [remaining qualification](RESOURCE_TENANCY_IMPLEMENTATION.md#remaining-acceptance).

The embedding host already owns the assembled App, its inference listener, original
authenticated execution headers and a trusted ACP launch descriptor. The library
example `apps/bitrouter/examples/owned_capture_handler.rs` shows a handler using
that entry, live update/permission streams, fresh native session, prompt, checkpoint
summary and explicit shutdown. It does not assemble a second Service or listener.
The shipped CLI remains Legacy and shares the ordinary controller factory.

## Authority and supported operations

The actual Service authenticates the original credential and exact recipient
binding. It pins one retained execution revision and its one-route closure. Both
original scopes and current effective grants must explicitly cover the tenant.
ExecutionSubmit, EvaluationSubmit, ResourceRead and CredentialUse remain required.
Scope and row provenance are storage associations, never authorization. A narrowed
or revoked grant cannot gain authority from the original credential or a native ID.

The profile requires Unix process groups, provider-configurable ACP and an advertised
`model` select option that confirms the literal retained route. The endpoint comes
from the same App's existing loopback listener and original bounded execution
headers. The trusted embedding host supplies the complete explicit child environment;
ambient variables are cleared. Provider configuration is excluded from raw capture.
No upstream secret is copied into a capture descriptor or checkpoint manifest.

Only fresh harness-native sessions and their same-controller operations are
accepted. There is no list/load/resume/fork/delete, arbitrary history adoption,
terminal login or tenant session HTTP API. Selected active experiments are refused.
The original R1 remains fixed across active configuration changes. Resource-scoped
capture grants, retained policy checkpoint publication and tenant deletion require
later consumers and authority design; a learned Checkpoint resource is not silently
reinterpreted as permission to read raw ACP evidence.

## Actual consumer and custody

The facade calls the shared AgentProcess → Controller → CaptureProxy → AcpClient
factory. Owned capture follows the controller gate so internally sent provider
configuration and direct session forwarding cross the same private checks. Its
one-time set/list bootstrap is unavailable after initialization. Ordinary Legacy
controller ordering and public APIs remain unchanged.

Current authority, handle cancellation and Service stop are checked at synchronous
child spawn and new forwarding under the existing authority snapshot lock. A fresh
request is checked again after the durable observation wait and before forwarding.
An already-admitted response or disconnect may finish its exact cleanup after
revocation; this does not authorize a new read, native session, model call or positive
permission answer. The public facade exposes one bounded command slot, not the inner
unbounded client queue.

Before the pinned SDK can accumulate lines, parse JSON or enqueue messages, owned
child stdout crosses a raw reader with a 64 KiB frame limit (including newline),
512 newline frames and 8 MiB cumulative input per controller connection. Byte/frame
allowances are never reset by reads, delivery or ACKs. A fixed 4 KiB scratch buffer
bounds rejected read overshoot; rejected chunks do not reach the SDK. JSON batches
may contain multiple messages, so the cumulative byte allowance also bounds their
queued representation. These are finite input bounds, not an RSS or performance
qualification. Serialized capture-event bounds below are a separate later check.

Exhaustion stops that same connection through its existing cancellation/reaper
owners. The rejection and final observed-data delivery share the authority snapshot
lock, so queued content cannot be forwarded after rejection. A disconnected capture
is marked interrupted even if the SDK reports a clean protocol close. Already-admitted
SQL and process cleanup remain owned until proven complete; no new pump or registry
is added. Legacy transport behavior is unchanged.

The existing shared host slot/OwnedTasks aggregate owns the capture lifecycle,
competing with native hosts. Its outer frame retains the port, exact pending request
and actor receiver, process control/reaper proof, controller join, client driver join
and initialized client. Client-driver startup returns ownership before the first
handshake await. The child reaper retains its Child outside caught cleanup work;
failed wait/group cleanup keeps that same owner, fences Service and never emits a
successful reap proof. Successful teardown requires the controller, client driver,
child/group and capture terminal append. No timeout aborts these owners. The host
cleanup deadline fences while continuing to poll the same work. Service retains the
existing host-before-pipeline/observation-before-actor shutdown order.

An initial process-group signal returning EPERM is deferred to the actual
direct-child wait and an absence-only group probe. Only ESRCH proves absence;
Ok and EPERM keep the same reaper pending under the existing host cleanup deadline.
The Child and stderr owner remain retained, and successful reap proof also requires
both the stderr task join and its inner I/O result. No additional group signal is
sent after the direct child has been reaped.

An admitted connection that stops before a child connects is durably interrupted,
not left recording or presented as a completed conversation. Uncertain transaction
outcomes retain the exact pending operation and owner. There is no automatic replay,
fresh operation ID or replacement process after an unknown result.

## Storage and passive checkpoints

No new migration is required. The participants use033's nullable exact namespace
and creator columns on connections, canonical sessions, capture events, checkpoints,
checkpoint members and checkpoint_evolution. They borrow the existing actor's
transaction and OperationBudget; no store owns another pool or begins a transaction.
Namespace, creator and exact host/session association predicates precede payload
loading. SQL transfer preflights and aggregate operation bounds limit decoding.
All033 Legacy NULL-ownership filters and worker dormancy guards remain in place.

A connection has at most512 observations,256KiB per serialized observation,8MiB
aggregate serialized content,16 native sessions and32 pending calls. Admission
reserves room for each pending maximum-size response plus the final interrupted
disconnect. Bounds stop new work before they consume cleanup space. Metadata is
bounded separately. Captured raw content is opt-in harness data; Core receives only
immutable observation references, digest, stopped watermark and scheduling cursor.

The actual checkpoint operation calls EvolutionScheduler's bounded borrowed pass.
Off does no discovery. Observe and passive Explore freeze only a complete prefix
whose latest session observation is an acknowledged prompt stop and which has no
pending call. A trailing update is not a new stopped boundary. The checkpoint,
member and cursor are written in one actor transaction. Exact replay checks its
entire closure before DML; absent or malformed closure is refused, not repaired.
Advancing a cursor also validates the prior immutable prefix against current refs.
Duplicate append ACKs reconstruct bounded correlation from the connection's durable
event prefix. Every event's required or forbidden session edge must match that
reconstruction, the final state must equal retained metadata, and all referenced
session heads must exist exactly. Stored NULLs or equal-head sibling edges cannot
choose which closure is required; malformed evidence is rejected read-only.

There is no judge dispatch, cost coverage claim, assessment selection, quota funding,
automatic delegation, activation or promotion here. `max_evaluations` is not reset
or consumed by passive observation. Those paid behaviors are not part of passive observation.

The separate explicit paid-assessment profile can select a static Generation
judge as an additional immutable configuration root. Only a checked checkpoint
call with that retained profile initiates its own bounded logical job and native
execution; ordinary capture does not discover work. The implementation reuses
this actor, the execution/receipt Registry and033's scoped tables. Current grants
are rechecked before send and assessment selection. Admitted usage and facts can
finalize after revocation without authorizing another attempt or a new selection.
See the revision-cumulative allowance in RESOURCE_TENANCY_M0.md and
RESOURCE_TENANCY_IMPLEMENTATION.md. Interrupted accepted jobs fail startup closed;
no row alone reconstructs authority or proves that a provider was unsent. This
source extension does not enable the full M2/M3 capability flags, and its private
unrun test sources are not qualification evidence.

## Explicit retained ACP Block profile

An explicit operator-imported `block_v1` route profile adds one bounded static
Block to this same authenticated ACP consumer in Explore mode. Without that
profile, the passive behavior above is unchanged. The two approved selectors,
credential references, measurement contract and numerical policy are retained
with the immutable configuration; no ambient Block catalog is adopted.

The real opener attaches a private opaque provider link. The existing host slot
keeps a weak association with its original live capture Binding. The HTTP caller
must match that opener's tenant, stable principal, original credential and pinned
configuration. A stored open connection or copied link cannot reconstruct this
capability after close or process death. The request retains the original Binding
through admission, original H selection and final send. It acquires no second
host slot and strips the private link before provider conversion.

A real pending ACP prompt with its fixed model selected is required. Original H
validates the complete capture edge before committing enrollment and RouteIntent
with the existing Block algorithm. Missing/ambiguous pending prompts, continuation
overrides or a stale original source are explicit refusals. Prior local work
keeps the preexisting-session baseline. A rule non-hit keeps an honest baseline
intent and supplies no conclusive trial score.

At an actual stopped checkpoint or a later selection, the same H writer rebuilds
the cohort from scoped capture, the effective checked Human/paid revision and
original 035 receipts. Selected paid feedback must match its original job,
accepted input, output, receipt digests and consumed history reservation in the
same transaction and cumulative budget. A stale or retracted judgement cannot
retain its old positive score. Changed evidence can withdraw unsupported adoption;
monitoring rollback retains priority and a rolled-back control stays terminal.

Receipt membership and tariff matching reuse the journal's opaque account and
credential-slot projection from the original retained installation, route and
credential. All five Target fields must match; an unpinned route supplies neither
membership nor known cost. Namespace, execution/revision and current-authority
checks remain separate. Subject-token cost is known only for eligible plain-text,
tool-free input with complete provider usage and unambiguous retained tariffs.
Nonzero cache buckets need explicit valid rates, including rates inherited from
the base tariff; explicit zero rates are valid, while unused buckets need
no rate. Missing dimensions, ambiguous tariffs or auxiliary work remain unknown.
If the profile permits paid assessment, whole-family cost stays unknown even
when its judge loses the Human-head CAS; missing or unselected paid work is not
free work. This is not settlement, a tenant spending limit or a new allowance.
The numerical learner can update this Block's scoped control only; it cannot
activate another configuration. Ordinary profiles remain unchanged; dispatch
eligibility follows the same current Service fences.

The first source profile is limited to one Block/two selectors, 64 enrollments,
512 intents and the existing cumulative H row/byte/time budgets. Native operational
experiment units are a separate producer and cannot substitute for ACP evidence.
Independent source review covers these corrections. The six tariff tests, new
target-matching test and existing opaque slot-projection test passed; strict
all-feature/all-target Clippy and the actual application/fixture build also
passed. After the narrow tariff and fixture corrections, all sixteen exact Block
parents passed serially without retries, alongside nine exact conformance tests,
formatting and strict workspace, all-feature, all-target Clippy on Rust 1.97.
The reviewed Unix CI job selects these sixteen parents and the seven separately
passed native-repair parents. This closes the finite local Block campaign, not
remote CI, final platform/feature checks or performance. Actual local host
acceptance is recorded separately. See
[remaining acceptance](RESOURCE_TENANCY_IMPLEMENTATION.md#remaining-acceptance).

## Source tests and qualification boundary

`harness::trajectory::owned::tests` covers mixed namespaces through real Legacy
consumers, exact replay and damaged closure inside active transactions, a real SQL
trigger rollback, Off/open-prefix behavior, late updates and terminal headroom.
`tenancy::tests::execution::native::capture` uses actual Service authentication,
actor custody and the public facade. Its ordinary tests cover grant intersection,
denial before storage/spawn and cancelled writer admission. Ignored process tests
use the separately built Rust `owned_capture_fixture`, actual ACP and a mounted App
listener: passive modes, fixed route/credential, shared native/capture capacity,
revocation, cancellation during initialization and failed partial initialization.
Additional real-child tests hold a Capture writer while supplying an unterminated
line, a small-frame flood or a cumulative-byte flood, then observe raw admission,
no forwarded updates, interrupted capture and the same owner/reaper/driver cleanup.
Replay regressions cover lost NULL edges, equal-head sibling substitutions and
forbidden disconnect edges, checking raw rows before rollback and afterward.
They never invoke Cargo or create a fake StartProof/authority.

Reaper process regressions use WNOWAIT to observe an exited but unreaped child,
then require the actual authenticated owner to finish reap, group absence and
capture closure. They also keep a same-group descendant alive after its leader
is reaped and verify that it prevents an absence ACK. Deterministic probe tests
require persistent EPERM or a live-group result to remain unacknowledged. The
macOS and Linux capture CI steps explicitly select all three ignored reaper
parents, including the helper under `harness::acp::up::owned::tests::process`.

Set `BITROUTER_OWNED_CAPTURE_FIXTURE` to that prebuilt absolute executable path when
running the ignored tests. No process test skips a missing fixture as success.
Local macOS qualification passed all 5,559 workspace tests, 12 doctests, strict
all-target Clippy, warnings-as-errors rustdoc, formatting and distribution checks.
All 13 separately selected process parents passed without retries. Reduced-profile
checks and earlier real PostgreSQL 17/MySQL 8.4 capture consumers also passed; see
[the implementation evidence](RESOURCE_TENANCY_IMPLEMENTATION.md) for their exact
boundaries and the isolated follow-up to one nextest LEAK marker. Platform CI and
performance remain separate gates. This slice adds no production dispatch or
strict-receipt availability.
