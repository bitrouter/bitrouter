# Resource tenancy implementation

This tracks implementation of the contracts in [M0](RESOURCE_TENANCY_M0.md).
The original baseline was `38ddfd231f053beae9b52a61443a3706688628e0`.
The stack now continues merged service/crate, native-agent and Registry v1 work
through main `25c8239049c0da80d08f3c152dc35710e62199bd`, followed by the
HTTP/2 (#994) and shared-concurrency (#995) changes.
All stages stay in the same stacked change. A checked stage requires runtime
evidence and independent review; fixtures alone do not satisfy runtime gates.

The integration preserves the current command-oriented CLI and independent
`core`/`harness` features; it does not restore the retired TUI. Ordinary Core
execution keeps complete context. Paid Choice is available only through an
explicit sealed owned profile, and skill-bound context retains its provenance
and cannot be omitted by that choice. Owned configurations continue to reject
ambient Registry bindings and tariffs; explicit retained routes use their own
credentials and exact rational cost evidence.

The current delivery boundary is independent review and final composed-head CI.
Historical validation below does not qualify the restacked source. M5 performance
qualification and deployment cutover remain separate, unperformed gates.

## Current local dispatch eligibility

The supported opt-in Unix owning-process SQLite profile now advertises revision-
bound dispatch and strict receipts when its existing live authority, storage and
admission fences permit execution. Tenancy remains disabled by default. Startup
must retain the exclusive database owner and complete initialization before
publication; discovery grants no route or credential authority. Unsupported
profiles and the existing stop, saturation and receipt fences remain unchanged.

Independent source review found no P1/P2 in this enablement slice. The current
source passed formatting and strict workspace/all-feature/all-target Clippy on
Rust 1.97. An isolated all-feature build and four actual Cargo-built bro HTTP/SDK
cases passed: H1 JSON with immediate durable receipt, restart and revocation;
prior-knowledge h2c SSE with a durable receipt at DONE; retained configuration
with source removal/wrong-key refusal; and owning-process lifecycle/restart.
The tests preserve the original deadlines and run without retries. These results
cover the all-feature local host; minimal feature profiles, final aggregate and
platform/backend/artifact gates, and matched-artifact M5 remain pending.

Historical stage records below describe their original candidates. Their earlier
blanket-disabled capability statements are superseded by this current eligibility
section; their recorded test evidence is not promoted to final qualification.
External writer retirement remains required before a real legacy cutover.

## Delivery stages

- [x] M0: frozen contracts, ownership inventory, migration example, adversarial
  scenarios and local benchmark profile.
- [x] M1a: public value types and bounded HTTP SDK with contract conformance.
- [x] M1b: persistent local authority, protected HTTP commands, roles/scopes,
  managed writers, CAS/idempotency, secret-once issuance and revocation fencing.
- [ ] M2a: owned drafts, immutable revisions, CAS activation/rollback, pinned
  execution envelopes, configuration validation/apply/read through SDK and CLI.
- [ ] M2b: ownership for all inventory families, including evaluation, evolution,
  scheduling, trajectories, continuations, credentials, caches and background work.
  Harness sessions and tool execution remain client-owned.
- [ ] M3: bounded strict execution facts, actual per-attempt targets/usage,
  durable completion, owned lookup, audit access and conservative crash recovery.
- [ ] Migration: additive schemas, explicit legacy mappings, quarantine of
  ambiguous owners, equal-name/hash isolation, writer fencing and recovery guide.
- [ ] Qualification: feature/dependency boundaries, local compatibility,
  cross-tenant and lifecycle races, HTTP/1.1/h2c JSON/SSE, slow clients and faults.
- [ ] Performance: five paired/interleaved repetitions of the frozen benchmark
  profile, external raw reports and recorded acceptance results.
- [ ] Final independent review and clean CI on the final candidate.

## Acceptance evidence

M1a adds distinct bounded identities, strict nullable CAS inputs, typed grants,
configuration and execution facts in the dependency-light contract crate. The
optional schema feature preserves the frozen wire constraints; its packaged
definitions are checked against the independent M0 schema. Exact integral JSON
spellings are accepted without floating-point rounding of revisions or usage.
The SDK implements all 16 control operations with bounded responses, explicit
installation binding, no redirects/retries, fresh capability discovery and
unknown-outcome recovery. It verifies returned owners, mutation targets,
activation revisions and pagination bounds before returning success.

Validation: 129 frozen fixtures (including 36 semantic rejections), eight direct
contract tests, 11 socket-level SDK tests including HTTP/1.1 and h2c, and the
existing contract/SDK/helper suites passed. The minimal feature profile passed
the contract and HTTP/1.1 tests. Strict all-target Clippy and an independent
adversarial review covered the public-contract/SDK stage; findings about schema
fidelity, integer representation, mutation-result identity and issued credentials
were corrected and re-reviewed. This establishes client/value conformance only;
live authority, dispatch and receipt guarantees remain pending below.

The M1b storage foundation adds empty installation, tenant, principal, binding,
credential and operation tables without assigning legacy owners. Identities and
issuer/subject pairs use exact comparison semantics on SQLite, PostgreSQL 17 and
MySQL 8.4; large valid grant scopes survive storage and interrupted migration
reruns. Real PostgreSQL/MySQL regressions and the frozen v7 database read test
passed. The latter permits new migration registrations while preserving every
historical timestamp and all 284 original record payloads across reads/reopening.
Independent review covered the migration and compatibility-test adjustment.
The serialized all-feature workspace run passed all 4,996 selected tests
(160 opt-in tests skipped), followed by strict all-target workspace Clippy,
12 passing doctests (one ignored), formatting and whitespace checks. This
foundation was qualified before adding the live authority runtime below.

The M1b runtime adds an opt-in local authority under the existing
protected control listener. Its runtime exposes the ten authority operations
(seven mutations, capabilities, credential status and operation recovery).
`control:tenancy` is independent of host read/reload grants. The initial profile
requires authenticated ingress, Unix file ownership, a literal SQLite database
path and local gateway state. It does not require a new Cargo feature or service.

One bounded command actor owns mutations and publishes immutable authority only
after committing the mutation and operation result. Token authentication uses
that current snapshot. Roles, explicit resource/pool scopes, delegation expiry,
managed writers and credential rotation are intersected with current bindings.
Secrets appear only in the first issuance response; replay and stored results
omit them. Ordinary/safety queues and retained operation IDs have separate
bounds. The saturation latch survives restart and cannot be reset by configuration.
The initial authority cache additionally bounds all retained principals to 128
and bindings to 256; the frozen per-tenant and live-credential ceilings still apply.

Writable SQLite pools retain shared file leases; a tenant authority requires an
exclusive lease before connecting or migrating. A dedicated canonical sidecar
avoids opening/closing database descriptors outside SQLite. Ambiguous nested
URIs and repeated mode/scheme parameters are rejected. Stop older binaries that
predate these leases before first cutover. Read-only host reports remain available.
Startup failures do not start authority/continuation workers; normal shutdown
drains admitted commands even after their HTTP callers disconnect. Legacy
continuation reconciliation is inactive in this control-only profile.

Runtime validation passed 5,026 all-feature workspace tests (160 opt-in tests
skipped), 12 doctests (one ignored), seven minimal-profile SQLite tests and a
subsequent 35-test minimal-plus-HTTP/2 tenancy run. The last run includes the
public operator-identity mapping, token rotation/restart, managed binding updates,
reserved-principal rejection and saved-result scope reduction regressions added
after the full run. Strict all-target workspace Clippy, formatting and whitespace
checks passed. Independent adversarial reviews covered authority, file ownership,
shutdown/startup failure handling and the operator identity contract. A real
Cargo-built host test verifies health, HTTP management, read-only reports,
cross-process writer fencing and durable restart; protocol tests exercise both
HTTP/1.1 and h2c through the public SDK.

Until M2/M3 complete, capabilities report
dispatch, revision-bound dispatch and strict receipts as false. Inference, native
embedding calls and MCP remain fenced; the process health endpoint stays usable.
These intermediate controls must not be interpreted as tenant inference support.

### M2 configuration foundation

The configuration foundation adds three empty, additive tables for explicitly
owned resource revisions, retained configurations and tenant active pointers.
It assigns no legacy records. Draft creation, exact revision reads and CAS
activation/rollback use the existing authority actor and operation journal;
publication follows commit. Current grants cover every retained dependency,
including credential slots, and activation additionally requires whole-tenant
authority. Configuration GET authenticates retained content before trusting its
dependency closure. Active configurations are verified before activation and
again at startup; a minimal build rejects activation/restoration of a document
that requires `core` learning facilities.

An explicit installation manifest supplies complete route/policy definitions and
static credential bindings. Definitions and compiled output are encrypted with
an operator-supplied stable key and bound to their qualified identities. Source
changes/removal cannot change a retained revision. No ambient provider registry,
credential lookup or policy file supplies a missing reference. Reusable encryption
primitives moved inside the application; the existing shared-store wire format
and its feature boundary remain unchanged. Compiler traversal, source expansion,
resolved route envelopes and concurrent management reads have finite bounds.

Compiler-v1 retains explicit targets and tariffs only. Registry v1 interface
metadata, registry pricing and compiled registry requirements are rejected at
the owned compilation, projection and retention boundaries; they are never
silently discarded or resolved through the host catalog. Existing compiler-v1
bytes remain unchanged. Block cost evidence compares exact nominal results
before bounded upward rounding and still requires explicit used-cache tariffs.

The base CLI adds `tenancy configuration validate/apply/read/activate/active`,
`tenancy capabilities` and `tenancy operation`. Local validation checks typed
document shape only; remote commands use the existing HTTP SDK with explicit
operation, installation and CAS identities. They do not discover a daemon,
assemble a service, open a local database or retry uncertain writes.

M2 remains incomplete: the compiled envelopes still need execution adapters,
live per-attempt eligibility and owner-bound background work. OAuth refresh,
native in-process caller integration and the other inventory families remain pending.
Successful draft preparation or activation does not advertise tenant dispatch.

Targeted qualification passed 79 minimal-plus-HTTP/2 tenancy tests, ten CLI unit
tests and two CLI process tests. Two Cargo-built host lifecycle tests passed,
including SDK activation, source-manifest removal, durable restart and rejection
of missing/wrong wrapping keys. PostgreSQL 17 and MySQL 8.4 each passed the new
configuration-store regression and the existing database-backend integration
test. These cover exact qualified identities, large UTF-8 values, immutable
conflicts, CAS and transaction usability after a rejected expected-absence CAS.
Independent reviews covered storage, compilation, authorization, deadlines and
process lifecycle. Findings about selector re-parsing, effective URL validation,
retained-content authentication, feature downgrade and read-only timeout fencing
were corrected and re-reviewed. Strict all-target workspace Clippy passed.
The subsequent all-feature workspace run passed all 5,087 selected tests
(161 opt-in tests skipped); it includes the configuration, CLI and process
regressions above together with the existing local, shared, core and harness suites.
Workspace doctests passed 12 tests (one ignored). The pure HTTP/1 minimal build
passed its all-target check; formatting and staged whitespace checks passed.

### M2 execution integration

The execution adapter selects a literal route from the exact submitted retained
revision. It uses the existing pipeline, protocol adapters, provider transports
and JSON/SSE renderers with a request-local profile. Host hooks, provider-name
credential plugins and mutable policy files are not inherited. Configuration
generation defaults enter canonical request fields before routing; explicit
request values take precedence and Decisions receives no generation defaults.
Compilation now checks the same decoded-work limits as retained-data reads before
committing a revision.

An internal execution capability binds the installation, tenant, authenticated
principal, ingress credential, retained revision and fresh execution ID. Current
grants are checked after queue/storage waits and immediately before every actual
provider send. Each send binds the exact retained candidate and authenticated
method, URL, headers and body, with a new attempt identity and an eight-attempt
ceiling. Static credentials resolve through qualified immutable slots. Repeated
correlation IDs never deduplicate inference. The submission deadline covers body
ingress and narrows each provider request's remaining time budget.

Atomic tenant/instance admission bounds active and queued executions. Its lease
survives non-streaming disconnects, stream preparation, prepared-result handoff,
stream delivery and tracked cleanup. Per-invocation pipelines use the existing
owned task tracker; shutdown drains these tasks before stopping authority.
Failed cleanup remains owned instead of being reported as a clean shutdown.
Known upstream 401/403 status remains authoritative if its diagnostic body fails,
so truncated or stalled rejection bodies cannot authorize provider fallback.

This integration remains production-fenced. Positive execution tests use a
test-only readiness flag; shipped capabilities still advertise dispatch and strict
receipts as unavailable. Owned OAuth, auxiliary provider work, native callers,
the remaining state inventory, durable attempt/terminal evidence and final
performance qualification remain pending. This section does not mark M2 complete.

The minimal-plus-HTTP/2 tenancy suite passed 133 tests, including actual H1/h2c
JSON/SSE exchanges, canonical token/temperature defaults across three provider
protocols, queued revocation/suspension/scope reduction, deadline-bound ingress,
provider-header disconnects, unread stream handoffs and bounded admission.
Upstream error regressions preserve 401/403 across truncated/stalled bodies and
redact private fixed-header echoes across Generation, Decisions and SSE.
Independent review findings about compile/read capacity disagreement, route-ID
collisions, stream preparation custody and fixed-header diagnostics were corrected
and re-reviewed. The all-feature workspace run passed 5,147 selected tests
(161 opt-in tests skipped). After two mechanical lint fixes, all 13 admission
and provider-header lifecycle regressions passed; independent review confirmed
equivalent behavior. Strict all-target workspace Clippy, 12 doctests (one ignored),
the pure HTTP/1 minimal all-target check, generated distribution checks,
formatting and whitespace checks passed.

Three separately built full, gateway and harness-only executables passed all five
opt-in profile-transition tests, preserving the 284 historical record payloads,
original migration timestamps, published policies, judge reservations, trajectory
cost guards and outbox state. The additive migration check also verifies the exact
currently registered schema without requiring the historical migration count to
remain frozen. All 12 artifact identity tests passed. This qualifies compatibility
of the execution integration, not production tenant dispatch or strict receipts.

### M2 immutable caches

Service-local caches retain authenticated execution revisions and static
credentials. Keys include the exact tenant/resource revision and the published
authority revision; installation, process instance and wrapping-key lifetime
are fixed by the owning service. Hits retain immutable bytes only. Current
credential, tenant and dependency grants are checked after waits and again at
each provider send. Management reads and startup continue to authenticate
retained storage independently of these execution caches.

Execution entries have a 64-entry/128 MiB allocation-payload budget; credentials
have a 256-entry/8 MiB budget. Fixed metadata, spare allocation capacity and
retained nested data are charged with checked arithmetic. Selected routes keep
the entire revision pinned, including unselected routes. Exported targets copy
header buffers so they cannot outlive the cache's accounting through shared
allocations. These budgets exclude allocator metadata, RSS fragmentation and
bounded transient decoding work; they are not process-memory measurements.
Misses share the existing eight fail-fast read slots. Pinned entries cannot be
evicted, capacity exhaustion does not create a waiting queue, and shutdown
fences further publication.

Read/cache capacity failures retain their typed 429 response; storage-integrity
failures retain their typed 503 response. Neither is misreported as a caller
grant denial. An execution that has reserved a provider attempt cannot claim
proven non-execution in that error response. Post-header streaming evidence
and durable terminal facts still depend on M3.

The minimal-plus-HTTP/2 suite passed 158 tenancy tests, covering warm hits with
all read slots occupied, cold-load pressure, authority changes, revocation,
expiry, same-name tenant/operator resources, restart corruption checks and
retained-pin eviction. Six real-process host lifecycle tests passed, including
a deterministic held-port collision and bounded retry of only the exact owned
child's listener-binding failure. Independent source review found no remaining
cache-stage defects after the shared-header correction. The subsequent all-feature
workspace run passed all 5,174 selected tests (161 opt-in tests skipped).
The provider-rejection fixture now connects to its exact reserved IPv4 listener,
so its deliberately short response timeout does not depend on localhost's
IPv6 fallback; it retains the same status and no-fallback assertions. Strict
all-feature/all-target workspace Clippy, 12 doctests (one ignored), the pure
HTTP/1 minimal all-target check, generated distribution checks, formatting and
whitespace checks passed. Cross-platform CI for this stage remains pending.
Cache eviction/destruction latency under authority publication locks remains
part of the pending performance qualification.

This stage does not enable tenant dispatch or mark M2 complete.

### M2 native execution and evaluation storage

The canonical native entry uses the same authenticated submission, exact retained
revision, capacity lease and tracked pipeline as tenant HTTP. Public caller
metadata, host prompt transforms, host cost readers and host Decision runtimes do
not supply tenant authority. Native routing has no invented inbound wire protocol:
the runtime derives an ordered chain from authenticated compiler-v1 source, then
requires every exact target and credential reference to occur in the same
selector/operation's retained approvals. Persisted compiler-v1 bytes and HTTP
selection are unchanged. Native derivation has separate cumulative work and
candidate limits; an unsupported native view does not invalidate its HTTP view.

Embedding callbacks run inside tracked execution with the request deadline and
bounded outcome cleanup. Returned context alternatives share a 16 MiB serialized
input budget and a 32-view limit before the shared planner copies them. Request
queueing cannot extend the submission deadline. Synchronous embedding callbacks
remain trusted process code and must not block. Owned input counting, provider
continuations, OAuth and auxiliary inference still require their own integration;
the current fences are not evidence that those facilities are complete.

Migration030 adds nullable installation, tenant and creator metadata to all four
evaluation tables. Historical rows retain NULL ownership and their original
payloads and keys. Internal scoped stores derive ownership from admitted execution,
use fixed-width physical keys while preserving logical DTO identifiers, and keep
creator provenance separate from the tenant namespace. Legacy owner APIs reject
tenant handles. Scoped predicates precede loading, metadata and parent relationships
are validated, and snapshots bind their namespace and immutable evidence closure.
Tenant reads are bounded to 4096 rows per collection and 32 MiB of cumulative SQL
cell/manifest bytes per operation; these are not allocator or RSS guarantees.

Core policy settlement writes observations through the admitted store. Learning
Off omits those observations. Static policies in Observe mode record outcomes
without granting exploration. Active experiments remain unavailable until their
tenant budget/reservation path is qualified; signed policy content is not rewritten
to disguise a different treatment. The evaluation exchange HTTP API, evolution,
other ownership families and strict durable execution receipts remain outstanding.

The final all-feature targeted run passed 236 tests; the minimal-plus-HTTP/2
run passed 189 tests. PostgreSQL 17 and MySQL 8.4
passed the migration and immutable-conflict scenarios, including scoped admission
reloads. Migration regressions reject dependent expression indexes, foreign keys
and incompatible index options before DDL; MySQL catalog escaping is checked
against actual stored expression and CHECK text. Unrelated objects and historical
record bytes survive both migration directions. SQLite resolves foreign-key
columns through its catalog, including single-quoted and case-varied identifiers,
before either migration direction performs DDL.

Independent review findings were corrected and re-reviewed: outcome callback
timeouts close subsequent send admission without losing billed error usage;
snapshot and compiler reads share one cumulative budget; MySQL admission inserts
reload through scoped queries; and only the selected active experiment is fenced.
An execution lease is allowed to outlive HTTP response delivery during tracked
cleanup, so the warm-policy regression waits for automatic release without
increasing capacity or forcing cleanup. The full workspace run passed all 5,228
selected tests (163 opt-in tests skipped). Strict all-feature/all-target Clippy,
12 doctests (one ignored), and the warnings-as-errors workspace rustdoc build
passed after removing a blank line between a doc comment and its declaration.
A fresh cross-component review found the SQLite foreign-key preflight gap; the
fix passed both final targeted runs and independent re-review. The full workspace
run preceded that final SQLite-only fix. Separately built full, minimum gateway
and harness-only executables passed 12 artifact metadata checks and five real
process transition tests. The historical-schema guard now permits only the exact
three nullable additions on the four Eval tables, preserving all original column
definitions and indexes; this test correction passed independent review. Historical
records, published policy/accounting, judge reservations and trajectory/outbox
state survive the tested profile transitions. The corrected artifact test passed
strict Clippy; distribution, formatting and whitespace checks passed. This stage
does not complete M2.

### M2 authority transaction custody

Management writes and credential-status reads now share explicit transaction
outcomes in the existing actor. Precommit write failures await rollback; successful
reads also end with confirmed rollback. A typed pool-acquisition failure can prove
that no transaction was created. Other BEGIN errors, consuming commit/rollback
errors and unexpected transaction unwinds cannot be treated as cleanup proof.
They fence authority and quarantine the same actor, retaining its database/file
ownership and active-job custody until process exit. No automatic retry or
un-fence is added. Confirmed late management commits still publish their durable
state while leaving authority fenced.

Credential reads carry their original five-second deadline and owned read permit
through the ordinary queue. Expired queued jobs are rejected synchronously before
polling storage. In-flight reads finish their current await and explicitly roll
back after a deadline; cleanup exceeding one second fences new work while the
actor keeps waiting. A timely confirmed read rollback does not fence authority.
HTTP cancellation drops only the response waiter. Shutdown closes new command
admission and requires explicit cleanup confirmation in addition to task drains.
A canceled shutdown waiter cannot release the actor or turn quarantine into success.

Targeted qualification passed 196 minimal-plus-HTTP/2 tenancy tests and 201
all-feature tenancy/host tests, including two real host lifecycle cases. Real
SQLite deferred-constraint commit failure and consuming rollback error regressions
cover the unknown-outcome boundary. Rust child processes verify file ownership
after normal service and pool handles close; a protected HTTP router test verifies
read capacity survives handler cancellation. Independent review found no remaining
actionable defects after fixing the expired-queue admission check. The final
all-feature workspace run passed all 5,242 selected tests (163 opt-in tests
skipped). Strict all-feature/all-target workspace Clippy, 12 doctests (one
ignored), the pure HTTP/1 minimal all-target check, generated distribution
checks, formatting and whitespace checks passed. Owned evaluation HTTP, atomic
ledger admission and required observation draining are addressed by the next stage.

### M2 tenant evaluation transport and required observations

The `core` feature adds seven tenant evaluation routes to the protected control
listener, documented in [the additive transport contract](RESOURCE_TENANCY_EVAL_HTTP.md).
Each request carries an installation header and an ordinary tenant credential.
The current credential and recipient binding must both cover the path tenant;
read and submit permissions remain distinct. Installation operators, receipt
audit credentials, legacy keys and resource-only grants cannot substitute for
this authority. Manual results use the authenticated stable principal and the
Generic evaluator profile. These imports do not start providers, activate policy,
or consume learning-job allowances.

The existing authority actor owns each composite transaction. Result insertion,
prior-result disputes and terminal admission are atomic; snapshot membership and
its immutable manifest share one repeatable-read transaction. Current authority
is checked after storage waits, before commit and before HTTP response handoff.
Exact replay appends no events. Postcommit deadline failure reports unavailable
and fences authority; a timeout after enqueue cannot claim no write. Explicit
freeze timestamps retain their original decoded spelling in the stored root.

Eight ingress leases cover bounded request bodies, queued work and response
delivery. Collections are limited to 1,024 rows, cumulative SQL/manifest work to
32 MiB and serialized responses to 8 MiB; overflow rejects the complete operation.
Latest-admission reads seek one scoped row without materializing historical events.
The independent SDK exposes the same seven operations without service dependencies.
After a mutation has been sent, failed confirmation has an explicit unknown
outcome and never triggers automatic retry or an invented operation identifier.

Admitted execution observations now use the same actor through the existing
pipeline registry. Each required observation retains its bounded prepared subject
and completion disposition. Queue exhaustion, rollback and missing confirmation
cannot become a clean drain merely because a recorder returned or its error was
logged. Cleanup authority survives credential revocation. Shutdown closes external
ledger admission first, lets admitted producers settle, then stops the actor.
Failure before any policy decision has a separate narrowly checked disposition.
Production dispatch and strict receipts remain disabled pending later stages.

The final all-feature workspace run passed all 5,297 selected tests (169 opt-in
tests skipped), and 12 doctests passed with one ignored. Strict all-feature,
all-target workspace Clippy, pure HTTP/1 minimal all-target compilation,
distribution checks, formatting and whitespace checks passed. Targeted checks
covered 49 evaluation tests, 223 minimal-core tenancy tests and 197 gateway-only
tenancy tests. The public SDK exercised all seven operations through the actual
protected listener over both HTTP/1.1 and h2c, including encoded logical IDs and
exact freeze timestamps. Independent review found no remaining actionable
defects after fixing a postcommit deadline race.

Five isolated composite transaction tests passed on each of PostgreSQL 17 and
MySQL 8.4. Replay assertions inspect the still-active transaction before rollback
so rollback cannot conceal accidental writes. These stronger assertions were
added after the full workspace run; both backend suites and strict workspace
Clippy passed again. CI selects all five tests explicitly and grants the MySQL
fixture account access only to its generated test databases. Earlier migration
and point-store tests alone do not establish these composite guarantees.

The same candidate fixes shared-state due scans that encountered a proven
PostgreSQL serialization abort. The existing bounded retry covers only typed
transaction aborts; uncertain commits and provider calls are not retried. A
real PostgreSQL regression failed before the fix and passed afterward, as did
the three store regressions, four single/dual-process HTTP/1.1 and h2c cases,
and the cross-replica h2c admission case.

### M2 native continuation and startup key custody

Controlled native Generation installs a request-local policy in the existing
private-context runtime. Opaque `btct2.` continuation and `btpo2.` origin artifacts
bind the installation, tenant, stable principal and exact retained route. The
executor supplies private proof of the actual candidate, credential reference,
authenticated request, attempt, prompt and terminal output. Public continuation
source fields cannot mint that proof. Caller-key rotation may retain the same
principal; equal upstream secrets do not substitute for a different credential
reference. Changed eligible targets may replay independently replayable history
but cannot receive another target's provider handle.

The native SSE bridge verifies the canonical prompt against the actual streaming
request before recording either source. A complete terminal must still agree
with the observed stream and the final canonical request. Failed artifact sealing
preserves received output and usage, records unverified evidence and exposes no
raw provider response ID. Token, origin-count, aggregate-origin and incremental
prompt-hash bounds limit the added work.

The existing configured wrapping key supplies a private domain-restricted crypto
capability. Migration031 adds one durable installation/key epoch. The owning
authority actor validates that epoch before importing resources, authenticates
retained and active configuration in the same transaction, and publishes readiness
only after acknowledged commit. Cancellation drops the startup waiter without
dropping SQL custody. Ambiguous completion and unwind retain the actor and file
owner. A missing or changed key rejects even when the retained catalog is empty;
a populated epoch cannot be downgraded away. PostgreSQL resumability requires a
permanent ordinary table, and identity comparisons remain exact on all backends.

The final all-feature workspace run passed 5,318 selected tests (170 opt-in tests
skipped). Targeted all-feature and minimal `core,http2` runs each passed 247 tests;
the minimal gateway passed 217. These include actual native two-turn/streaming
consumers, startup cancellation, consuming-commit failure, bootstrap unwind and
file-owner retention after ordinary handles close. Migration030, migration031
and the populated database integration passed on PostgreSQL 17 and MySQL 8.4.
CI runs the isolated epoch test before populating its backend fixture.

Strict workspace and minimal-core Clippy, pure HTTP/1 all-target compilation,
12 doctests (one ignored), warnings-as-errors rustdoc, distribution, formatting
and whitespace checks passed. Independent reviews found no remaining actionable
defects in native binding, startup custody, backend regressions or the merged
Eval/lifecycle integration. HTTP continuation mappings and reconciliation are a
subsequent consumer integration. This stage does not enable production dispatch
or strict receipts, store harness session history, or complete M2.

### M2 retained trajectory and atomic operational evaluation

Selected retained progress guards now use an execution-bound adapter and the
existing authority writer. A separately derived correlation key binds the exact
installation, tenant, configuration revision, route and policy. Principal and
credential rotation do not split that profile's canonical history; each row still
retains its original creator. Migration032 adds nullable ownership projections to
the five existing trajectory tables without assigning historical rows. Legacy
reads, publication, metering joins and pruning exclude owned records.

An acknowledged start installs its settlement obligation before any fallible
reply or deadline check. Start, route and optional guard are validated as one
batch. Required terminal history, built-in operational Eval and delivered outbox
commit together through the same transaction owner and shared operation budget.
Missing terminal closure fails read-only validation, including inside the open
transaction; replay cannot tentatively recreate missing Eval rows. Foreign
secondary-key collisions leave the PostgreSQL transaction usable while exact
scoped winner validation refuses adoption.

Before committing another start, the writer reserves complete terminal event
and Eval/outbox allowances for every outstanding request. Capacity exhaustion
rejects before another provider send and preserves management availability.
Learning Off retains operational history without an Eval/outbox obligation.
The [retained trajectory profile](RESOURCE_TENANCY_TRAJECTORY_PROFILE.md) records
the finite bounds and unsupported cost, experiment and retention settings.
Retained integrity remains independent of build capabilities: a gateway without
`core` can read an authentic inactive guarded revision while rejecting its
creation, activation, dispatch and active restoration.

Qualification passed 487 all-feature targeted tests, 281 minimal `core,http2`
tests and 224 pure HTTP/1 gateway tests. A separately built core producer and
no-core consumer passed the real encrypted-file transition. Actual H1/h2c socket
tests cover disconnects before headers and during streaming, revocation before
begin and after commit, and shutdown waiting for acknowledged rollback. SQLite
tests detect tentative replay writes across 18 intact/damaged closure cases.
PostgreSQL 17 and MySQL 8.4 each passed migration032, scoped immutable storage and
three compound tests spanning 17 isolated fault/replay campaigns. Their generated
databases were removed after verified actor, transaction and pool cleanup.

Independent reviews found no remaining P1/P2 defects in capability separation,
complete terminal reservation, socket custody, scoped replay, PostgreSQL
conflicts, backend fixtures or the merged integration. SQLite downgrade tests
preserve schema, rows, indexes and migration ledger when persistent/TEMP views
or external triggers prevent safe column removal.

Final workspace qualification passed 5,359 all-feature tests, with 176 opt-in
tests skipped, plus 12 doctests with one ignored. Strict all-target workspace,
minimal-core and pure HTTP/1 gateway Clippy, warnings-as-errors rustdoc,
distribution and formatting checks passed. Five real artifact tests passed with
separately built default-full, minimum-gateway and harness-only executables,
preserving historical records, resident ownership, learned state, reserved judge
charges and trajectory/outbox state across profile transitions. The schema
allowance retains the original composite-primary-key bytes and inserts only the
literal nullable additions in SQLite's actual column position.
Nextest reported one output-pipe leak warning in an existing status-only test;
its isolated rerun passed without that warning. The cause was not established.

Production tenancy remains the local SQLite profile; these server-backend
regressions do not enable distributed authority. Dispatch and strict-receipt
capability flags remain disabled, and M2 is not complete.

### M2 Legacy capture and evolution namespace

Migration033 adds nullable installation, tenant and creator projections to the
nine existing capture, canonical-session, checkpoint, observation, assessment
and evolution tables. Historical bytes, public entity shapes, logical IDs and
existing indexes remain unchanged. Every Legacy read, mutation and count
requires all three ownership columns to be NULL before loading payloads,
including child rows and partially populated ownership metadata.

Recorder and evolution initialization use conflict-safe insertion followed by
scoped current reads; a marked conflicting key cannot be adopted or updated.
Non-returning inserts require one affected row. Evolution saves use revision
CAS, and assessment selection cannot overwrite a foreign head. Canonical
deletion, dependent checkpoint invalidation and cached judge-job deletion use
one existing transaction and reject foreign graph edges before committing.

The daemon's Legacy evolution launch path exits before discovery or task
creation when tenancy is enabled. Ordinary local launch and drain behavior
remain available. This supplies isolation for existing consumers; it does not
provide the authenticated owned capture/controller path or paid evaluation.
Migration preflight covers all nine tables before DDL, supports partial resume
and refuses populated ownership or incompatible dependent objects on downgrade.

Qualification passed 457 all-feature targeted tests. The same affected unit and
checkpoint CLI coverage passed 89 minimum-gateway, 177 core-only, 104
harness-only and 197 core+harness tests, with strict all-target Clippy for each
profile and the all-feature workspace. PostgreSQL 17 and MySQL 8.4 each passed
the isolated nine-table migration and actual mixed-namespace consumer tests;
all 11 existing MySQL snapshot regressions also passed. Generated fixture
databases were absent after verified cleanup.

Independent review found no remaining P1/P2 defects in the integrated Legacy
consumers, graph edges, insertion conflicts, launch gate, transactional deletion,
migration safeguards or artifact schema allowances. All 5,372 workspace tests
passed, with 178 opt-in tests skipped, and 12 doctests passed with one ignored.
Warnings-as-errors rustdoc, distribution, formatting and whitespace checks passed.
Five real artifact tests passed using separately built default-full,
minimum-gateway and harness-only executables. The pinned historical database
preserved its rows, original column/constraint bytes, indexes and migration
timestamps across all profile transitions; only the exact nullable ownership
additions were accepted.
Dispatch and strict-receipt capabilities remain disabled; owned capture and
paid learning remain separate work.

### M2 authenticated HTTP continuations

The Responses consumer resolves and publishes continuations through the existing
tenant authority actor and continuation registry. Each lookup binds installation,
tenant, stable principal and the exact retained route revision. A new invocation
selects its own immutable configuration and current grants; the prior execution
revision is provenance, not authority to reuse stale defaults or policy.
Credential rotation can preserve the principal, but equal secret bytes cannot
substitute another credential resource. Raw provider IDs and reserved continuity
defaults cannot bypass the owned lookup.

The actual authenticated executor produces private attempt and output evidence.
JSON and SSE require validated terminal evidence and mandatory post-hook output
comparison. Provider-backed resume without a reconstructible causal prefix remains
possible, but cannot authorize history detachment. Publication follows the existing
required-delivery handshake. Compensation, lookup read permits, pending generations
and producer shutdown remain owned until the actor acknowledges their exact
transaction outcome. A private ticket-and-origin-bound acknowledgment distinguishes
confirmed compensation from uncertain cleanup; generic finalizer failures remain
sticky. An admitted request may finish after caller-key revocation, while tenant
suspension or deletion independently prevents new publication transitions.

Migration034 adds nullable ownership projections to `provider_continuations`
without assigning historical records. Legacy reads and writes require NULL
ownership, and owned payloads use the installation epoch with authenticated
metadata. Exact-byte CAS and conflict handling apply on every backend. Downgrade
refuses populated provenance and incompatible schema dependencies. Production
startup runs bounded owned reconciliation after initialization; unknown transaction
outcomes retain the actor and canonical file lease.

All 5,426 all-feature workspace tests passed, with 182 opt-in tests skipped.
Twelve doctests passed with one ignored. Warnings-as-errors rustdoc, distribution,
formatting and whitespace checks passed. Reduced-profile continuation/migration
runs passed 158 minimum-gateway, 161 core and 158 harness tests, with strict
all-target Clippy for all three profiles and the full workspace. The final
mailbox/wire additions passed three all-feature tests and five minimum-gateway
tests including the existing authenticated-request digest checks.

Real PostgreSQL 17 and MySQL 8.4 each passed the isolated migration and actual
continuation-consumer regression. Five artifact tests passed with separately
built default-full, minimum-gateway and harness-only executables, preserving the
pinned historical records and schema through profile transitions. Actual H1/h2
connection-disconnect tests verify both continuation compensation and the separate
trajectory/Eval terminal obligation. Isolated child tests retain file ownership
after real BEGIN, consuming COMMIT/ROLLBACK and caught-unwind failures. A subsequent
serial-libtest handshake correction was reproduced before fixing and passed all
five relevant tests in both all-feature and pure H1 gateway builds; strict
workspace Clippy passed again. The correction changes test framing only.

Independent review found no remaining P1/P2 defects in the merged continuation
consumer or the serial-child correction. See
[the HTTP continuation profile](RESOURCE_TENANCY_HTTP_CONTINUATION.md) for request
binding, output evidence and lifecycle details. These checks qualify this consumer
only. Strict execution receipts, remaining native/capture consumers, auxiliary
inference and final performance gates remain outstanding. Dispatch and
strict-receipt capability flags remain false.

### M3 owned execution receipt implementation

Migration 035 adds immutable owner-qualified execution receipts and a correlation
index. Admission atomically reserves active capacity and bounded completion
storage. The existing execution registry retains each admitted invocation through
provider work, required finalization and durable receipt completion; dropping an
HTTP waiter does not release that custody. Attempts bind the actual authenticated
request and current retained authority before sending. Original provider evidence
supplies usage independently of canonical conversion and local output failure.

The existing authority writer owns receipt transactions and identical-candidate
reconciliation. Connected JSON and final SSE success wait for acknowledged
receipt commit. Earlier stream metadata remains provisional, and the final
positive delivery witness still requires the caller's actual body poll. Negative
terminal evidence is published synchronously even if its asynchronous finalizer
is never polled. Continuation compensation does not acknowledge unrelated receipt
or producer cleanup. Unknown operations retain their owners and never replay
inference.

Protected receipt reads recheck current authority after waits and validate the
stored owner and integrity. Ingress is bounded by 128 concurrent uploads, 64 MiB
reserved bytes, 16 MiB per request and a fixed 60-second deadline. Completion
reservations, disk-space thresholds and unknown-outcome limits are detailed in
[the receipt implementation profile](RESOURCE_TENANCY_RECEIPTS.md).

Qualification passed 5,514 all-feature workspace tests (186 opt-in tests skipped),
12 doctests (one ignored), strict all-target workspace Clippy, warnings-as-errors
rustdoc, formatting and distribution checks. Reduced-profile receipt,
continuation and migration coverage passed 152 minimum-gateway, 158 core and 155
harness tests, with strict all-target Clippy for each. A subsequent backend-only
test-import scoping correction changed no executable logic or assertions; the
reduced suites and final workspace Clippy qualify that correction separately
from the recorded full-suite source snapshot.

Real H1/h2c tests cover 128 upload slots, reserved-byte pressure, absolute 60-second
upload deadlines and consumed requests above 16MiB, with an exact 16MiB positive
control. Actual SQL COMMIT/ROLLBACK errors, caught unwind and incremental final
frame tests retain the real owner when cleanup is unknown. Cancellation and
callback deadlines preserve original provider usage and synchronous negative
terminal evidence. Required-observation failures retain their own cleanup
obligations even when the independent receipt commits successfully.

PostgreSQL 17 and MySQL 8.4 each passed the additive migration 035 and real receipt
store campaigns, including immutable conflicts, rollback, reconnect and scoped
lookup. Independent reviews closed the identified integration and fixture
issues without relaxing production bounds or uncertainty handling. These are
consumer and storage checks, not measurements of filesystem exhaustion or
production throughput.

The subsequent capped APFS campaign observed actual filesystem ENOSPC and
SQLite FULL at the original consuming COMMIT. It passed owner/capacity and file
lease retention, absence of terminal success, and post-exit integrity and lease
reacquisition checks. The earlier immutable receipt survived unchanged. This is
the observed macOS failure path only; Linux, power-loss/fsync durability, write
amplification and production performance are not established by it.

This is an implementation slice. Broader filesystem qualification, recovery,
remaining owned consumers and final performance gates remain tracked separately.
Production dispatch, revision-bound dispatch and strict-receipt capability flags
remain false; M2 and M3 are not marked complete.

### M2 authenticated native harness

The additive in-process native session facade derives its owner from the App's
actual Service and current authenticated binding. It retains the original
configuration revision and scope ceiling across close/reopen, credential rotation,
child creation and wakeups. Each model step re-enters the existing controlled
pipeline with fresh admission and current grants; an idle session holds no
execution lease or retained cache pin. The facade requires both `core` and
`harness` and adds no HTTP Session resource or CLI command.

Migration 036 adds eight nullable ownership fields to the existing native journal
without assigning Legacy rows. Scoped metadata and actual SQL payload length are
checked before loading journal bytes. Existing checkpoints, exact acknowledgments,
artifacts and tool-start records remain one atomic journal. Unknown-write readback
accepts only the complete prior journal or exact attempted replacement; it never
replays a provider or tool effect.

The existing Service owns initialization, the actual MCP client/process/reaper,
journal transactions and shutdown. Dropping a caller does not abandon the owner.
Revocation stops new effects while admitted checkpoint acknowledgments and cleanup
retain their exact origin-bound access. A missed cleanup deadline preserves the
real owner and fences authority. Legacy managed CLI and HTTP entry points retain
their explicit boundaries.

All 5,540 all-feature workspace tests passed (191 opt-in tests skipped), together
with 12 doctests (one ignored), strict all-target Clippy, warnings-as-errors
rustdoc, formatting and distribution checks. Reduced consumer/migration suites
passed 109 minimum-gateway, 112 core, 109 harness and 130 core-plus-harness tests,
with strict all-target Clippy for each profile. The separately invoked real MCP
process parent passed its seven cleanup/cancellation scenarios.

PostgreSQL 17 and MySQL 8.4 each passed migration 036 and the real native journal
consumer, including credential rotation, rollback and a conflicting foreign row.
The PostgreSQL length projection correction was exercised on the actual backend.
Later changes were confined to tests, CI and documentation; the qualified
production source remained unchanged.

Five real artifact tests passed using separately built default-full, minimum
gateway and harness-only executables. All 284 historical record payloads, original
schema definitions, indexes and migration timestamps survived the profile
transitions. The schema guard accepts only the eight exact nullable additions
and rejects twelve malformed variants; all 12 artifact identity tests passed.

Independent composition review found no remaining P1/P2 in this slice. See
[the native harness profile](RESOURCE_TENANCY_NATIVE_HARNESS_PROFILE.md) for the
binding, journal and lifecycle contract. Owned capture, paid learning, auxiliary
provider work, broader recovery and final performance remain separate gates.
Production dispatch, revision-bound dispatch and strict receipts remain disabled;
this consumer does not mark M2 or M3 complete.

### M2 authenticated passive capture

The local `ControlledSession::open_authenticated` facade reuses the assembled
App, its authenticated loopback inference listener and the ordinary ACP controller
factory. One current authority pins the retained revision and exact route closure.
Fresh native sessions, prompts, live updates, permission replies, passive checkpoint
summaries and explicit shutdown use the same bounded capture owner. The shipped
CLI retains its Legacy profile; no tenant session HTTP API is introduced.

Raw stdout is bounded before SDK line parsing and queueing. The actual child,
process group, stderr copy, controller, driver and admitted transactions remain
owned through cancellation and shutdown. An uncertain cleanup fences Service.
An initial process-group EPERM is deferred to actual child wait and an absence-only
probe; only ESRCH confirms absence, and no group signal is sent after reaping.

Owned observations and stopped-prefix checkpoints use migration 033's existing
namespace columns and the same authority transaction. They preserve Legacy NULL
filters, validate complete replay closures and reserve terminal-response space.
Off, Observe and passive Explore do not dispatch paid assessments or publish a
new learned policy.

All 5,559 all-feature workspace tests passed (206 opt-in tests skipped), together
with 12 doctests (one ignored), strict all-target workspace Clippy and
warnings-as-errors rustdoc. The final formatter-only test correction and macOS
CI budget change passed formatting and distribution checks. The updated
core-plus-harness profile passed strict all-target Clippy and all 156 selected
tests. Earlier minimum-gateway, core and harness profiles passed their respective
116, 125 and 118 selected tests and strict all-target Clippy.

All 13 separately selected real process parents passed serially without retries,
including actual exited children, live descendants, stderr ownership and file
lease reacquisition. The raw small-frame flood also passed twenty independent
post-fix runs without retries. Earlier real PostgreSQL 17 and MySQL 8.4 capture
consumers passed namespace, replay and reconnect checks; the qualified backend
implementation remained unchanged by subsequent reaper and test corrections.

The full workspace run retained one nextest LEAK marker on an unchanged startup
reconstruction test. One isolated run on the same source passed without the
marker. The original cause remains unconfirmed; this is not a memory-leak
diagnosis or a claim that an intermittent condition was fixed.

Independent source and correction reviews closed the identified ownership,
replay-fixture and feature-gate findings without relaxing the production limits.
Windows excludes Unix-only file-owner consumers and retains explicit unsupported
configuration/file-owner refusal coverage. The prior Windows run exposed those
fixture assumptions. At `185164c75924edaccce21cd92b9f2a29602e89b0`, CI
reported 67 successful checks, eight skipped checks and three failed checks.
The Linux and macOS library jobs failed the consuming-COMMIT capture fault
parent; the third failure was the aggregate Linux check. This failure exposed
repolling of an already-completed oneshot receiver during cleanup. The next
implementation slice retires the receiver before processing its result and
retains pending work and sticky uncertainty on an ambiguous transaction.
New platform CI on that correction remains required. macOS receives
a 90-minute outer CI budget after actual cold fixture/test compilation consumed
about 59 minutes of the previous 75-minute job. Per-test deadlines and retries
are unchanged; an incomplete earlier CI run is not counted as a pass.

See [the passive capture profile](RESOURCE_TENANCY_CAPTURE_PROFILE.md) for the
actual consumer and lifecycle boundaries. These results do not measure throughput,
RSS or provider cost. Paid learning, auxiliary provider work, broader recovery
and final performance remain separate gates. Production dispatch,
revision-bound dispatch and strict receipts remain disabled; M2 and M3 remain
incomplete.

### M2 owned OAuth and provider input counting

The current slice adds encrypted live OAuth slots with immutable seed lineage,
serialized generation changes and bounded refresh ownership. Its supported
consumer is a retained HTTP Messages route to Claude Code. Native OAuth remains
explicitly refused before I/O. A separate native Generation profile supports
provider input counting with static Responses credentials, including observed
streaming and one bounded context reduction followed by a second count.

Both consumers retain auxiliary facts separately from model attempts and token
usage. The existing terminal transaction commits those facts with the receipt.
An unclean auxiliary result cannot acknowledge successful delivery; cleanup may
join an already-known durable terminal result without converting it to success.
No public authority type, endpoint, feature or runtime crate is added.

Independent review identified a real count-to-stream mismatch: the expected
generation request was rendered with `stream=false`, while native observation
sent `stream=true`. The new streaming regression failed with the digest mismatch
while its non-streaming control passed. The correction renders both requests
with the executor's existing streaming choice and retains complete request,
credential and current-authority checks.

Final local qualification passed 5,619 all-feature workspace tests (210 opt-in
tests skipped), 12 doctests (one ignored), strict workspace/all-target Clippy,
formatting, warnings-as-errors rustdoc, distribution and tenancy-contract checks.
All 13 separately selected capture process parents passed serially. Runtime
campaigns used zero retries. The focused auxiliary campaign passed 56 tests,
including observed streaming with one/two counts, actual non-streaming
NativeSession counting, same-slot refresh races, and truncated/stalled rejected
refresh bodies. Independent source and regression-evidence review closed the
streaming mismatch and the identified OAuth coverage gaps.

Minimum HTTP/1.1 gateway, HTTP/2 gateway, core+HTTP/2 and core+harness+HTTP/2
passed strict all-target Clippy and respectively 406, 409, 459 and 483 selected
tests. Reduced PostgreSQL 17 and MySQL 8.4 profiles each passed strict Clippy
and three real migration037/OAuth/compound-receipt campaigns. Existing unrelated
fixture databases were preserved. Final source qualification also includes the
optional-dependency fixture corrections exposed by the first reduced build.

Earlier failures remain separate evidence. One full run was interrupted for the
streaming correction after 890 passes, two interrupted tests and 4,722 not run.
A later run passed 5,618 tests but timed out in the existing MCP handshake
cancellation test; its stderr showed a nested fixture build. Supplying the
already-built examples directory, as Unix CI already does, passed that exact
test and the subsequent full run without changing source, assertions or test
deadlines. This does not establish the original timeout's cause. The completed
oneshot correction also passes the real capture process campaigns; CI on the
new commit remains required after the preceding head's failures.

See [the auxiliary profile](RESOURCE_TENANCY_AUXILIARY_PROFILE.md) for the exact
consumer, custody and bounded-response rules. These functional results do not
establish full recovery or performance. Production dispatch, revision-bound
dispatch and strict receipts remain disabled; M2 and M3 remain incomplete.

The 16 operations and 35 runtime scenarios in `contracts/resource-tenancy/` are
the minimum conformance inventory. Every family in `inventory.json` needs a
runtime ownership decision and a regression test before M2 is complete.

Ordinary local execution keeps its existing zero-configuration behavior. New
hosted operations fail closed on missing authority, stale revisions or storage
failure. No anonymous fallback, implicit tenant merge, cross-tenant content-hash
lookup or automatic retry of uncertain inference is permitted. Capabilities are
advertised only after their corresponding dispatch guarantees exist.

Ordinary local performance budgets are at most 3% throughput regression,
TTFT p99 growth of `max(5%, 1 ms)`, and RSS growth of `max(5%, 4 MiB)`.
Strict execution evidence must retain at least 85% of ordinary authenticated
throughput, with TTFT p99 growth of `max(10%, 2 ms)` and receipt commit p99 at
most 20 ms on the frozen local reference. Measurements use 15 s warmup, 60 s
sampling and 30 s drain at concurrency 1/16/64/128. These are pending gates,
not claims established by M0 smoke tests.

## Owned paid-assessment slice under development

The private slice follows the accepted budget interpretation in
[M0](RESOURCE_TENANCY_M0.md#paid-assessment-budget-interpretation): cumulative
accepted logical jobs per immutable configuration revision, no refund after
durable acceptance, and a new budget for a new revision rather than a tenant
spending limit. The budget period is immutable-revision cumulative.
Observe requires an explicit imported evaluator binding and the checked native
checkpoint action; it enables no automatic judge discovery or activation.

The implementation boundary is a static retained Generation judge, the existing
actor/H owner, execution admission/Registry/receipt, bounded assessment evidence
and immutable assessment selection. Experiment assignment, candidate algorithms,
automatic publication and the remaining inventory are separate work. Existing
Legacy bytes and compiler-v1 inputs remain readable. No new public DTO, endpoint,
permission or parallel registry is introduced. This paragraph records the
authorized implementation boundary, not passing validation or feature completion;
M2/M3 capability flags remain false.

## Boundaries

Public DTOs stay in `bitrouter-ai`; HTTP transport stays in `bitrouter-sdk`;
authority, persistence, compilation, dispatch and receipts stay in
`apps/bitrouter`. Base gateway includes local tenancy and strict receipts;
`core` adds scheduling/learning; `tui -> harness -> mcp` and optional,
default-on `http2` remain unchanged. No new runtime crate or external database
is required. Identity-provider login, commercial policy and accounting remain
external services. Single-instance operation is the acceptance target; distributed
coordination is reserved without implementing it in this change.


## Native conclusive assessment source

The local authenticated NativeSession review/submit/retract path uses a distinct
`recorded-native-quality-evidence-v1` projection. Its private CompletedRun decoder
requires the normal full journal, ACK and artifact validation plus exact native
experiment Unit/Step, execution receipt and delivered trajectory/Eval edges.
It never constructs an ACP SessionIdentity or Checkpoint from native data.
Assessment history reuses the existing scoped revision/head tables with a
qualified native Unit parent and immutable versioned source edge. Current stable
principal, grants, original scope ceilings, explicit head CAS and the existing H
transaction owner apply to all actual facade calls.

A current complete Human rubric supplies a separate checked quality contribution
to cohort acquisition. Full credit maps to Pass, complete lower credit or a known
severe violation to Fail, and incomplete quality to Inconclusive. This mapping
is explicitly versioned and uses the existing full-credit workflow semantics.
The original operational projection and frozen Eval root remain unchanged. The
learner reacquires the source closure and effective assessment revision after
computation; its retained checkpoint records real conclusive/unavailable counts
and a separate effective-head digest. Existing measurement tuples for ACP-v2
are unchanged; unsupported projection versions fail closed.

This first source profile deliberately requires the current inspectable completed
run and its original epoch. Closing the original owner can feed the existing
stopped-source learner; replacing the run, reopening it at another epoch, or
interpreting a recovery archive cannot silently retarget its judgement. Native
paid assessment uses an explicit `NativeSession::assess_run(checkpoint,
expected_revision)` call on the same authenticated owner. The retained experiment
route must name a qualified static Generation evaluator; the selected subject
and evaluator credentials must both be static, with no count, OAuth or recursive
paid side effect. A caller-supplied evaluator, ambient judge discovery and
non-experiment native source are unsupported by this slice.

Durable acceptance pins the actual completed source in the original native
binding, increments the immutable configuration revision's cumulative logical
job counter, and reserves terminal history before any judge request. Version4
paid records retain the native evidence packet separately from the bounded
receipt seed; versions1/2/3 keep their existing serialization. The host polls the
paid producer in its existing task, permits a Human correction while the judge
is awaited, and prevents a new run from replacing the pinned source. Actual035
receipt/output facts survive a lost head CAS; a Human head cannot be overwritten
by the paid result. Known usage and accepted budget are never erased or refunded.

A historical terminal audit checks the retained projection against its original
immutable Unit/Step, receipt and publication closure. Recovery of an accepted
unfinished job additionally requires actual native resource/journal close ACK,
the exact old Registry stop-set and the existing single StartupOwner claim. A
retained projection alone cannot authorize cleanup or execution. The startup H
repairs known output as unselected or a missing terminal as strict Unknown and
Interrupted, with no paid resend. The current-run rule for live review and
contribution remains unchanged. This adds no table, writer, registry, permission,
remote endpoint or automatic activation.

Targeted local runtime qualification now covers the native assessment consumer,
including original paid-future custody when a concurrent Human transaction
becomes unknown. The original producer remains alive through its known provider
result; revocation cannot grant a new send or discard accepted usage. The
21-test paid/Human/callback/custody run passed with the normal test-thread stack.
The earlier failing ownership regression is retained separately from this pass.

A real four-run budget regression passed credential rotation, a newly created
immutable revision, reactivation of the original revision and database reopen.
The original revision remained spent; only the new revision admitted another
logical job. Ten actual stopped-v4 recovery parents passed: nine independent
negative changes to genuine accepted records each refused two fresh bootstraps
without repairing rows or retaining the file lease, and the clean control
preserved its original known receipt through two normal bootstraps. Corruptions
cover owner metadata, native evidence, receipt digest, mirrored body, close ACK,
cleanup epoch and Registry stop-set membership. Generic startup rejection is
not claimed as proof of every internal validation branch.

Three separately compiled gateway, core-only and harness-only libtests each
passed both ACP and native paid startup campaigns. Every campaign uses an
original physical database from the full producer, a clean inactive-draft
control, explicit reduced-feature verification and a consumer ACK. Pending work
is refused without changing its facts or resending provider requests; a missing
test match cannot count as success. These six public parents now have dedicated
Unix CI entrypoints. Local results do not imply successful CI on both platforms.

Seven new-tool admission regressions passed, including actual experimental
workspace reads, verification and ordinary-session compatibility. The one- and
eight-outstanding-tool allowance cases each execute one actual tool; they do
not claim eight concurrent tools. Separately, both full 8 MiB Choice receipt
campaigns passed with application test optimization and unchanged debug
assertions, product bounds and operation deadlines. The corrected two-pending
campaign took about 18.9 minutes; its fixture observation wait is separate from
the original H deadline.

The actual 8 MiB Human-history boundary has targeted coverage. The cumulative
32 MiB Human-read frontier remains unqualified: the measured eight-tool completed
source plus the full permitted Human-only history is still below that ceiling.
Reaching that exact maximum is not an independent required workload; the ceilings
do not promise that every dimension can be saturated together. Final acceptance
still requires the shared cumulative OperationBudget, checked charges before
payload loading, next-read reservations before DML, and the actual 8 MiB history
regression on the final candidate. This is not a proof of global unreachability.
A fixed 31-small-tool diagnostic stopped during model selection after 22 actual
provider requests. Its retained prefix necessarily needs at least 61,789 bytes
for the next trajectory terminal reservation, beyond the unchanged 60 KiB cell
limit. The exact runtime rejection site was not logged, so this source-derived
bound is not a recorded throw-site claim. No completed source or Human-read
boundary result was produced; the saved failure cannot be relabelled as one.
No further probe, limit increase or fabricated source is proposed.

After the internal message-layout and recovery-context lint corrections, strict
workspace Clippy with all features and targets passed on Rust 1.97. All 51
ordinary native experiment regressions then passed serially, including the
current paid recovery, integrity, Human, authority and actual-tool consumers.
Opt-in reduced/capacity campaigns retain their separately recorded results.
Delivery stages remain open pending final qualification; current dispatch
eligibility is described above.

## Choice capacity functional qualification

The two native Choice capacity consumers qualify the real receipt frontier,
refusal before provider send, retained pending outcomes and clean release. Their
build uses the existing `dist` profile with explicit debug assertions and integer
overflow checks:

```text
--cargo-profile dist
--config profile.dist.debug-assertions=true
--config profile.dist.overflow-checks=true
```

This replaces the application-only test-profile optimization for these two
consumers. Both Unix platforms, all features, original workloads and assertions,
serial execution, zero retries and every operation/campaign deadline are
unchanged. Cold compilation remains separate from the bounded runtime campaign.
Record the actual source, compiler, effective profile and test executable with
each result. Earlier application-only profile failures remain failed observations;
a result from the replacement profile cannot relabel them.

This is a dist-derived test executable with additional checks, not evidence that
its bytes or configuration equal a shipped binary. It establishes functional
capacity only after the original consumers pass. It does not demonstrate a
production speedup or replace the separate matched-artifact M5 measurements,
their development build profile, baseline or performance budgets.

## Remaining acceptance

The reviewed [ACP Block profile](RESOURCE_TENANCY_CAPTURE_PROFILE.md#explicit-retained-acp-block-profile)
rebuilds effective Human/paid evidence, can withdraw unsupported adoption, and
matches receipts using the original opaque configured-slot identities. Strict
subject-token tariffs do not establish whole-family paid cost. The six tariff
regressions and new target-matching regression passed, as did the existing opaque
slot-projection test. The composition containing these corrections and native
paid-repair tests also passed strict workspace, all-feature, all-target Clippy on
Rust 1.97 and the actual application/fixture build. All seven native paid-repair
parents subsequently passed on the same source: genuine missing035 producers,
actual BEGIN/ROLLBACK/COMMIT failure, caught unwind, cancelled waiters, late known
ACK and two fresh restarts retain the required owner/accounting proofs without
provider resend. After the narrow tariff and fixture corrections, all sixteen
exact Block parents and nine exact contract-conformance tests passed serially
without retries on one frozen source, together with formatting and strict
workspace, all-feature, all-target Clippy on Rust 1.97. Those finite consumer
campaigns are closed; earlier failed runs remain failures. The reviewed Unix CI
job now selects all sixteen Block and seven native-repair parents; installation
of that workflow is not a remote CI or final platform pass.

The remaining sequence is finite:

1. Reconcile the final aggregate against the frozen evidence map for all ten
   inventory families, sixteen operations and thirty-five scenarios. The map now
   links the finite consumers to recorded evidence or explicit unsupported/
   quarantine refusals; it is not itself a final runtime pass.
2. Verify the final migration schemas, artifacts, CLI reports and archive
   counts/digests. A real cutover additionally requires external writer-retirement
   evidence; the offline verifier cannot establish it from attestations alone.
3. Run final workspace, feature/platform, supported backend, artifact, docs,
   distribution and contract gates on one composed candidate. Earlier targeted
   results remain evidence for their pinned source, not the final aggregate.
4. Complete the remaining minimal and HTTP/2 feature profiles for the supported
   production path. The four actual all-feature compiled-host HTTP/SDK cases
   passed as recorded above; test-only readiness is not production evidence.
5. Execute the matched-artifact M5 campaign and final independent review/CI.
   Unsupported profile intersections and arbitrary quota cross-products do not
   add new requirements to this sequence.

The external Node 24.10.0/pnpm 10.25.0 runner with the freshly rebuilt `38ddfd2`
baseline pin passed build, typecheck and 462 checks (461 passed, one actual-Service
opt-in test skipped). These are runner qualification results, not a benchmark or
release result. The
baseline is a new development-artifact rebuild, not recovered historical bytes;
matching candidate artifacts, real capabilities, receipts/capacity/OTLP checks,
observer overhead and the five paired performance rounds remain required.
