# Resource tenancy implementation

This tracks implementation of the contracts in [M0](RESOURCE_TENANCY_M0.md).
The baseline is `38ddfd231f053beae9b52a61443a3706688628e0`; the implementation
continues the existing service/crate, HTTP/2 and shared-concurrency refactors.
All stages stay in the same stacked change. A checked stage requires runtime
evidence and independent review; fixtures alone do not satisfy runtime gates.

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
ledger admission and required observation draining remain subsequent work.

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

## Boundaries

Public DTOs stay in `bitrouter-ai`; HTTP transport stays in `bitrouter-sdk`;
authority, persistence, compilation, dispatch and receipts stay in
`apps/bitrouter`. Base gateway includes local tenancy and strict receipts;
`core` adds scheduling/learning; `tui -> harness -> mcp` and optional,
default-on `http2` remain unchanged. No new runtime crate or external database
is required. Identity-provider login, commercial policy and accounting remain
external services. Single-instance operation is the acceptance target; distributed
coordination is reserved without implementing it in this change.
