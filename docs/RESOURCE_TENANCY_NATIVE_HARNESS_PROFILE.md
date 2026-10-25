# Authenticated in-process native harness profile

This additive local Rust profile connects `NativeSession::open_authenticated`
to the existing App, CoreSession and persistent native harness. It requires
both `core` and `harness`. It adds no HTTP endpoint, CLI option, SDK method or
public tenancy Session resource. Production tenant dispatch and strict receipts
remain disabled. The existing managed CLI, BRO stores and managed HTTP adapters
retain their Legacy boundaries.

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

## Qualification boundary

Positive tests use the existing qualified tenant fixture flag and real provider
sockets. This slice passed the full workspace suite, four reduced feature
profiles, the real MCP child campaign, PostgreSQL/MySQL migration and consumer
tests, and five compatibility tests with three separately built executables.
See [the implementation tracker](RESOURCE_TENANCY_IMPLEMENTATION.md) for counts
and scope. These checks qualify this consumer, not production tenant dispatch.
Owned capture, auxiliary provider work, paid learning, broader recovery and final
performance remain separate gates; all production dispatch capabilities stay false.
