# Authenticated in-process native harness profile

This additive local Rust profile connects `NativeSession::open_authenticated`
to the existing App, CoreSession and persistent native harness. It requires
both `core` and `harness`. It adds no HTTP endpoint, CLI option, SDK method or
public tenancy Session resource. Dispatch and strict receipts follow the supported
opt-in, single-instance Unix SQLite
[Service eligibility](RESOURCE_TENANCY_IMPLEMENTATION.md#current-local-dispatch-eligibility).
The existing managed CLI, BRO stores and managed HTTP adapters retain their Legacy
boundaries. Earlier slice qualification statements below retain their original
scope; source-pinned local checks do not establish final aggregate, platform or
performance acceptance. See [remaining qualification](RESOURCE_TENANCY_IMPLEMENTATION.md#remaining-acceptance).

## Binding and execution

The facade takes the App, authenticated headers, local session name, workspace,
tool mode and HarnessConfig. Workspace and harness configuration are trusted
in-process host inputs. The App's own Service derives installation, tenant and
stable principal from its current credential and exact recipient binding.
CallerContext labels and protocol OwnershipGrant fields cannot construct the
sealed binding. No bearer is persisted.

Names contain 1–128 ASCII letters, digits, underscores or hyphens. Authentication
uses the existing dispatch binding and header limits: 128 values and 64 KiB.
The opener checks retained R1 and the effective execution-submit and credential-use
permissions before reading the scoped journal or workspace. Because opening has
no model selector, this profile requires the entire retained revision's native
Generation route closure to fit the effective credential/binding intersection.
The closure has at most 4096 distinct resource references. Resource-only scopes
can qualify when they cover that closure.

The immutable descriptor preserves R1 and the original scope envelope. Current
credential and binding state, expiry and resource coverage remain necessary for
new work. A broader later credential cannot enlarge the durable envelope.
Authenticated close/reopen may rotate a key for the same stable principal;
it preserves the journal's session identity, revision and original envelope.
Using another Service's binding fails the process-origin check.

Every model step, including children and wakeups, goes through the existing
controlled native pipeline and freshly admits against exact retained R1. Active
revision promotion does not change a live session. Canonical selection has no
forged wire protocol. Host prompt rewrites, ambient credentials and cost readers
do not replace tenant selection. Execution IDs and provider leases belong to
individual steps. An idle session holds neither an execution lease nor a retained
cache pin. New input, steering, signals, queue activation, collaboration, reconnect,
material requests and actual model/tool dispatch check the binding again.

## Journal and lifecycle

Migration 036 adds nullable ownership metadata to `bro_managed_sessions` without
assigning Legacy rows. The physical key includes installation, tenant, stable
principal and local name. Creator provenance is not an authorization predicate.
All owned metadata reads, bounded payload reads and CAS writes are scoped before
I/O. Legacy access requires all eight ownership columns to remain NULL.

The existing checkpoint, exact ACK, artifacts and tool-start records remain one
atomic journal row, wrapped in a versioned descriptor. There is no second journal,
transcript copy or session registry. Metadata and actual SQL cell length are checked
before loading a payload, within the native store's existing 128 MiB bound. One
repeatable-read actor transaction supplies each load or CAS. Initial insert uses
an explicit insert execution without a hidden reload.

The existing Service owns at most one authenticated local host slot, shared by
local host consumers. It uses its ordinary
authority writer, H transaction supervision and owned-task lifecycle. The native
command queue holds at most one command. Journal queue/work deadlines are five
seconds; cancellation cleanup has a separate thirty-second bound. A missed
cleanup deadline fences authority and retains the owner. An error, timeout or
joined worker alone cannot establish cleanup.

Asynchronous admission and outcome callbacks are descendants of the same
registered execution task owner. Their request/outcome reply deadlines bound
only the waiter; cancellation does not drop a pending Core checkpoint ACK. New
preparation checks the original deadline and current authority before its first
poll and after completion, so a late admission reply cannot authorize a send.
Already owed outcomes keep their exact callback and original cleanup deadline.
Unwind or an unresolved callback prevents clean drain and retains the owner.

Authentication precedes journal and restore-artifact reads. Workspace discovery
and the real Core handle stay in the owner frame before their fallible awaits.
The actual MCP client, child and reaper ownership are retained before discovery
starts, including failure before a NativeResources value exists. Startup checks
current authority before instructions/skill work, after waits, and at MCP process
or request admission. Confirmed discovery rejection releases its workspace only
through the owner; unknown cleanup retains both the client and the host slot. Pending MCP service
close futures remain in that same client across cleanup deadlines, preserving
the actual join handle rather than calling close again on a consumed handle.
Caller cancellation signals the owner instead of abandoning initialization,
provider work, tools or checkpoint acknowledgement. Releasing a host slot follows
confirmed resource shutdown, durable journal release and workspace-fence release.

Revocation stops new effects while admitted result delivery, checkpoint ACKs,
cancellation and release remain possible. Cleanup access is bound to the admitted
row, exact grant and origin. Unknown-write readback accepts the previous exact
journal or the revision and digest of the complete attempted replacement; matching
only a checkpoint is insufficient. It never retries a provider or tool effect.
COMMIT uncertainty and unwind preserve H's quarantine and file-owner retention.

Downgrade refuses populated metadata and preflights dependent indexes, constraints,
views and triggers before any DROP. While any ownership column remains, SQLite
conservatively refuses all persistent and connection-local views or triggers,
including unrelated objects. Remove those objects before downgrade; token matching
does not resolve SQLite dependencies. A fully completed downgrade remains a no-op.
MySQL partial DDL resumes only after exact catalog validation.

## Bounded stopped-owner reconciliation

The authenticated facade has a private local Unix/file-SQLite reconciliation
path for a narrowly checked direct-HTTP epoch. A connector-created pool bundle
consumes one exclusive canonical acquisition claim across all lease clones.
Ordinary raw-pool constructors, a new instance UUID, elapsed time and an empty
new Registry do not create this proof. Normal bootstrap and current fixed-R1
credential/resource checks must succeed before recovery is considered.

A private V2 envelope records provenance and an immutable profile digest. An
ineligible Opening marker is acknowledged before MCP discovery, including when
reopening a released journal; it retains the complete prior journal. The first
profile requires empty MCP configuration, no tool history, Learning Off, no paid
assessment, and no OAuth/Count auxiliary work. Other unreleased native owners or
owned capture history without a post-reaper proof prevent reconciliation. V1
released rows keep their ordinary behavior; V1 unreleased rows are not upgraded
by guessing their effect history.

Real parent/child receipt seeds are written with the actual Choice intent ACK.
An existing terminal remains byte-for-byte unchanged. A missing terminal receives
one conservative Unknown fact using its original installation, tenant, principal,
execution, correlation and R1: no invented attempt, quantity or target, Unknown
provenance, and incomplete usage. A completed child receipt is not the missing
Decisions answer. Core records Interrupted for an unacknowledged Choice outcome.
Uncovered in-flight Generation and auxiliary work remain refused. A pending Choice
may retain the exact completed built-in entry, inactive HTTP continuation and
static selector-lookup sequence. All three records must name the same covered
parent execution, with exact indices and matching successful outcome ACKs.
Unfinished, reordered or additional preparation callbacks remain ineligible.

The existing H writer commits missing terminals and the exact native row/ACK
replacement atomically, within one cumulative 4096-row/32-MiB operation budget.
It uses the existing receipt byte, free-space and Unknown limits, without an
execution slot for a predecessor it did not admit. Ambiguous transactions retain
the real pool, reservation and owner. A confirmed read of some receipt cannot
replace unknown transaction cleanup or the exact whole-row ACK.

Recovery reconciles and stops prior work: it does not resume model steps, replay
outputs, run tools, or automatically activate queued roots. The prior workspace
claim does not prove a complete activity clock. For a nonterminal predecessor,
the first repair records `SessionSnapshot.activity_gaps[run_id]` with its exact
durable head and known lower bound.
The retained `active_ms` is a lower bound and no `RunActivityReconciliation` is
invented. Repeated retirement preserves that same gap; ordinary restoration
cannot resume it with a supplied clock value. The gap and first restore event
agree, and history validation rejects erasure or rebasing. A terminal cancelled
run keeps the uncertainty when reopened. A new root starts a fresh clock, while
the bounded immutable gap map preserves the old run even in a native journal
that stores only its latest checkpoint.
The prior workspace claim stays reserved until the repair and stop transitions
are acknowledged.
Retirement metadata preserves the exact predecessor across a crash between the
atomic repair and workspace activation. Only a later explicit new Run, with
current authority and fresh admission, can send new work. This is not proof that
a remote HTTP request stopped or incurred no cost, and it does not widen the
Legacy CLI or remote managed-session contracts.

This recovery addition still requires independent source review, compilation and
real process/fault qualification. The preceding native-profile qualification
does not qualify this new recovery branch; no capability flag is enabled.

## Qualification boundary

Positive tests use the existing qualified tenant fixture flag and real provider
sockets. This slice passed the full workspace suite, four reduced feature
profiles, the real MCP child campaign, PostgreSQL/MySQL migration and consumer
tests, and five compatibility tests with three separately built executables.
See [the implementation tracker](RESOURCE_TENANCY_IMPLEMENTATION.md) for counts
and scope. These checks qualify this consumer, not production tenant dispatch.
Owned capture, auxiliary provider work, paid learning, broader recovery and final
performance remain separate gates; all production dispatch capabilities stay false.

## Owned context Choice profile

Ordinary Core execution retains complete context and does not invoke a host
decision model. Only a sealed owned Choice profile enables the bounded candidate
planner below. Skill-bound evidence always stays complete, with its original
provenance; a Choice answer cannot authorize its omission or substitution.

The first owned Choice composition requires both `core` and `harness`. It is
consumed by the authenticated local Rust NativeSession facade through the existing
`context_views_v1` planner. It does not qualify all Core adapters, managed HTTP,
BRO/CLI entrypoints, or production tenant dispatch.

An operator-imported Generation Route may carry `core_choice` with
`profile: "choice_v1"`, an exact five-field child Route reference, and the existing
materialized DecisionPolicy. The child is a hidden dependency: one canonical
Decisions target, one static API-key credential, no child policy, exploration or
recursive profile. It must not also be an exposed configuration root. Authenticated
open checks the full retained closure before journal or artifact reads. The
profile and its child remain fixed at R1 through children, wakeups and reconnect;
active configuration changes cannot replace them. An absent profile retains
conservative preparation and does not enable the ambient host DecisionRuntime.

The policy allows at most 32 optional candidates, 2048 bytes per excerpt, 128 KiB
per translated request, a 16 MiB context target and 256 recent groups. Both the
planner request and the actual JSON-escaped native Decisions request must fit.
Stable tail reduction removes optional candidates until both fit; unjudged source
stays in full. If the routing-only request does not fit, no paid child is admitted.
Rubric names, text, null descriptions and string labels are preserved. Required
instructions, opaque continuity and atomic tool groups retain their existing
protections. A rejected answer cannot authorize omission, and original provider
usage is not replaced with a zero or a successful Core interpretation.

Each paid Choice obtains a fresh execution, ordinary admission slot and 035
receipt, separately from the Generation step already holding its slot. Admission
is fail-fast and never self-queues: a capacity of one rejects that preparation;
two permits a single parent and its child when no other reservations compete.
Choice uses the existing run decision bound; it does not consume a learning
`max_evaluations` unit. It is not a Generation model attempt or input counter.
Current authority, the source revision, Core grant, steering and run budget are
checked at the actual send gate after credential waits.

The same native journal stores exact parent/child references and execution IDs,
the translated request commitment, intent and outcome commitments. There is no
second journal, session registry or idle execution lease. A receipt ACK and a Core
outcome ACK are independent obligations. Cancellation or revocation stops new
work while admitted outcome delivery and cleanup remain possible. The existing
parent task set retains the driver and original control through both ACKs.
Unknown COMMIT, unwind or a missed cleanup deadline fences authority and retains
the actual owner. Lost ACK reconciliation checks the complete retained journal
and submitted outcome, never only the checkpoint head, and does not resend work.
The journal admits at most 4096 Choice links, 8 KiB per link and 8 MiB total links;
receipt validation shares an 8 MiB cumulative read budget with a 1 MiB per-receipt
ceiling before typed cloning.

The retained envelope's private format discriminator is 2 when an authenticated
optional route composition is present. The compiler identifier and seal AAD stay
unchanged. Absent composition fields retain the exact v1 serialized spelling;
older readers reject the new authenticated tag instead of silently ignoring it.
Independent imported assessment and Choice profiles occupy separate fields and
must retain separate route and credential proofs when composed. Their joint
consumer qualification is a separate integration gate.
