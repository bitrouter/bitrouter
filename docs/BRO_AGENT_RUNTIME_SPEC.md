# BRO agent runtime MVP

Version: **v0.2**. Updated: **2026-10-01**.

Status: **MVP scope confirmed in product design; implementation contract for
review; implementation is in progress and the full MVP is incomplete.** R0/R1 work has begun;
DTOs, storage guarantees, recovery procedures, and limits remain review items.
Updating this document does not complete R0 or any implementation phase.

Product source: [003 — BRO Agent Runtime Design and Implementation, v0.2](/Users/kelsen/Documents/bitrouter/product-engineering/bit-router-orchestrator/003-BRO-Agent-Runtime-Design-and-Implementation.md).
The product document defines the scope; this file translates it into engineering
requirements. If they disagree, reconcile them before implementation rather than
combining incompatible requirements from different revisions.

BRO baseline: `b4294b316a319031c09744b9c9a785cbecc65550`, with the current
worktree inspected on 2026-10-01. Codex reference:
`799324821d36a822923cee7814d3b80f7ec3cf99`, verified in the local clone.
The baseline inspection preceded implementation. Current changed behavior and
phase-specific checks are tracked in [runtime implementation evidence](BRO_AGENT_RUNTIME_IMPLEMENTATION.md).
Neither the baseline nor partial phase checks prove full MVP completion.

## 1. Decision and document authority

Build a minimal, extensible Rust coding agent inside `bitrouter-orchestrator`,
using Codex Rust as an execution/state/failure reference. BRO owns its small
runtime, uses BitRouter SDK model execution, and retains existing BRO tools.
No `codex-core` dependency or Codex subprocess replaces native execution.

The central model is **Thread → Turn → Item**. Each provider request is a
**model step**. Attachments observe and control server work; they do not own it.

The v0.2 MVP includes bounded tool concurrency, explicit queueing and steering,
persistent execution facts, and safe restart recovery. These replace v0.1's
sequential-tools-only, busy-rejection-only, and process-local-only scope.
Start still rejects a busy thread; enqueue and steer are separate operations.

For future native-runtime development, this spec supersedes:

- [Native agent server spec](BRO_NATIVE_AGENT_SERVER_SPEC.md).
- [Shared session server spec](BRO_SHARED_SESSION_SERVER_SPEC.md).

Their bodies preserve historical decisions and phase lists, not additional
requirements. Statements there that defer concurrency, queue/steer, persistence,
or recovery do not constrain this MVP. The
[implementation record](BRO_NATIVE_AGENT_IMPLEMENTATION.md) describes the earlier
runtime and recorded checks, not proof of this refactor.
[DEVELOPMENT.md](DEVELOPMENT.md) describes current code;
[CLI.md](CLI.md) describes delivered command behavior.

Product documents 001/002 retain workflow goals and automation evaluation;
003 v0.2 enlarges the native baseline delivery scope. Router document 004 retains
one `bro` and crate ownership; 005 retains longer-term state/context/cache design.
This runtime does not settle Jev's role or introduce adaptive orchestration.
Do not claim an end-to-end cost/latency/accuracy benefit without experiments.

## 2. Scope

One `bro` binary hosts the server and native CLI/TUI clients. Native, authenticated
HTTP, and inbound ACP share one runtime, permissions, history, and durable state.
Explicit external ACP harnesses retain their session/context/tool ownership.

| Area | Required MVP behavior |
| --- | --- |
| Conversation | Persistent Thread; at most one active Turn per Thread |
| Input | start, enqueue, steer; bounded FIFO, targeted cancellation, explicit queue resume |
| Tools | read, glob, grep, write, edit, shell; see [base-tool contract](BRO_BASE_TOOLS_SPEC.md) |
| Concurrency | Bounded shared reads; workspace-exclusive writes, shell, verification; ordered barriers |
| Model | Fixed Thread model/effort baseline; immutable snapshot for each SDK request |
| Output | Live assistant and stdout/stderr; authoritative bounded Item records |
| Permissions | Server-owned read_only, ask, allow_effects |
| Persistence | Admission, full responses, calls/results, queue, steering, approvals, budget, terminal facts |
| Recovery | Restore history/pending state; continue only from confirmed safe boundaries |
| Lifetime | Disconnect detaches; restart changes epoch, not durable Thread/Turn identity |

Initial deployment is a trusted operator in authorized workspaces. A directory
and tool policy are not OS sandboxing. Shell uses host process authority.
Untrusted tenant isolation requires separate design and evidence.

Deferred: arbitrary effect replay, transparent adoption of old interactive
processes, PTY/interactive stdin tools, automatic compaction, history editing/forks,
native subagent orchestration, adaptive policy, runtime plugins, generic hooks,
generic schedulers, client-executed tools, and a first-party Web UI.
SDK routing, fallback, providers, and metering remain in their existing path.

## 3. Baseline implementation and required changes

These are pre-refactor findings from the inspected baseline. Consult the runtime
implementation record for changes made since that inspection, not this table
as a claim that the old behavior still applies after each phase.

| Current source | Current behavior | Required change |
| --- | --- | --- |
| `service.rs` / TaskService | Mutex-protected in-memory tasks, workspace leases, TaskTracker, approvals, bounded snapshot/observation | Durable Thread/input state; short ordered transitions with commit acknowledgement; recovery ownership |
| `agent.rs` / run_with_approvals | New user-message history for every task; sequential call loop | Turn runner over retained context; explicit model step, scheduling and common settlement |
| `agent.rs` / record | Sends events to an mpsc channel; ignores send failure; sender does not await service commit | Separate provisional progress from authoritative transitions; commit success gates all execution |
| Call identity and bounds | used_call_ids spans an entire run; steps counts model requests and tools | Step-qualified provider IDs, distinct Item IDs; separately bounded model steps and calls |
| Cancellation/budget exits | Can return after adding assistant calls but before producing all results | Settle every committed call before continuation; record not-started and uncertain effects honestly |
| `context.rs` | Clones SDK messages, builds Prompt, checks serialized bytes | Persistent reconstruction, pairing validation, context versions, immutable request snapshot and cleanup reserve |
| `tools.rs` | Separate declarations/allowed/read_only/dispatch matches; schemas parsed inside execute | Static registry as common source; validate before approval; resource metadata and bounded scheduling |
| Approval timing | Approval receiver timeout uses elapsed run time | Persist pending/resolved state; fence attempts/epoch; exclude only pure approval wait from active duration |
| Verification | Runs after Agent report in service; absent check is Unavailable | Verification Item through the same permission/exclusive path; not_requested distinct; evidence in future context |
| Local / HTTP | Local contract v4; task API over same service; task/instance identity | Versioned Thread controls and history; old task behavior projects onto one new runner |
| App storage/host | App assembles SeaORM DB/migrations; host constructs TaskService without runtime storage | Dedicated runtime records and transactions using app-supplied storage resources |
| Native / ACP | Native task client; existing ACP drives external harnesses | Continuous native controls plus inbound BRO ACP bridge, with client evidence |

Do not feed snapshot text or the evictable event deque back as conversation.
Do not treat router metering/trajectory records as an execution recovery journal.
The current runner already waits for full model output before tool execution;
retain that boundary and add validation, capacity reservation and durable commit.

## 4. Codex reference and intentional differences

Read relevant pinned source and tests before implementing each subsystem.
Preserve attribution if copying code. Naming similarity is not behavior evidence.
All paths below are relative to the pinned clone's `codex-rs/`.

| Reference | Adopt | BRO boundary |
| --- | --- | --- |
| `app-server-protocol/src/protocol/v2/thread_data.rs` | Thread/Turn/Item identities and lifecycle | Small BRO types, no complete app-server DTO port |
| `core/src/codex_thread.rs`, `core/src/session/input_queue.rs` | Explicit admission, targeted steering, pending inputs | Durable BRO future-Turn FIFO is separate from Turn-local pending input |
| `core/src/session/turn.rs`, `core/src/stream_events_utils.rs` | Model/tool loop, cancellation, full output boundary | SDK execution; no streamed-call early effects |
| `core/src/session/step_context.rs` | Immutable settings/tools captured per request | Small ModelStepSnapshot, no MCP/discovery/realtime subsystem |
| `tools/src/tool_executor.rs`, `core/src/tools/orchestrator.rs` | Declarations/dispatch and central permission decisions | Static registration; three profiles; no Guardian/sandbox upgrade retry |
| `core/src/tools/parallel.rs` | Concurrency qualification, cancellation and call association | Bounded workers and ordered read/exclusive barriers; parallel does not imply read-only |
| `core/src/context_manager/history.rs` | Legal history, separate model and presentation views | SDK Message and BRO Item, not a Responses-only wire model |
| `protocol/src/items.rs`, history projection | Start/delta/terminal and reconstructable history | Bounded authoritative records; deltas may be evicted |
| `rollout/src/recorder.rs`, `core/src/session/rollout_reconstruction.rs`, `core/src/session/daemon_recovery.rs` | Commit, reconstruction, limited recovery | One transactional authority; no JSONL plus DB dual authority or arbitrary exactly-once claim |

Keep BRO read/search/write/edit schemas and bounded outputs. Codex exec_command,
write_stdin and apply_patch are not required substitutions. BRO edit retains
unique non-overlapping replacements, content recheck, temporary write and diff.
Those checks do not guarantee safety against external concurrent writers.

## 5. Ownership and modules

```mermaid
flowchart TD
    Native[Native CLI and TUI] --> Contract[Common client operations]
    ACP[ACP stdio bridge] --> Contract
    HTTP[Authenticated HTTP adapter] --> Contract
    Contract --> Service[ThreadService state owner]
    Service --> Store[Transactional execution store]
    Service --> Runner[TurnRunner]
    Runner --> Context[ContextBuilder and ModelStepSnapshot]
    Runner --> SDK[BitRouter SDK]
    Runner --> Tools[Registry permissions and bounded scheduling]
    Tools --> Workers[Tool workers]
    Workers --> Service
    Service --> Observe[Snapshots history and events]
```

Orchestrator owns execution state, input control, context, scheduling, approvals,
leases, settlement, recovery decisions and observation. App server supplies
trusted callers, workspace grants, config/credentials, storage and listeners.
App clients select targets and translate operations/events; TUI edits/renders.
ACP bridge is an adapter. SDK owns request preparation, routing, providers,
fallback, usage and settlement. BRO and SDK must not both execute one local call.

Refactor inside the existing crate first: service.rs for Thread/input/state;
agent.rs for Turn/model steps; context.rs for legal prompt reconstruction;
tools.rs split only as needed into registry/permission/scheduling/handlers;
small Item and execution-store modules when they have real consumers.
Use concrete types/enums and only necessary storage interfaces. No public
re-exports, unused framework, generic resource graph or backend plugin system.

Each Thread has one state-commit owner. Model/tool workers return facts; they do
not independently mutate conversation or Thread control. Commit authoritative
state in order before acknowledging it or publishing completion. The owner must
remain responsive while model/tool/approval/client I/O waits. Never hold a global
state lock across these waits. Service tracks and joins every owned worker.

Prefer existing app database assembly; local deployment uses SQLite. Runtime
admission, idempotency, queue, call states and results share one transactional
authority. Orchestrator defines consumed store operations; app supplies resources
and migrations. Exported logs are not another source of truth. Backend support
requires its own evidence; existing DB connectivity is not recovery proof.

ModelStepSnapshot captures model/effort, instructions, context version, exact tool
registry/version, and remaining bounds. Calls use the snapshot that advertised
them. Future policy may change snapshot construction, not an in-flight request.

## 6. Identities and state transitions

| Object | Meaning |
| --- | --- |
| Thread | Durable conversation, caller/access scope, canonical workspace, settings, permissions, context, history, queue and execution |
| Turn | One accepted unit of work; stable ID allocated at start/enqueue admission |
| Item | Stable BRO message/activity identity, invocation, origin, output and lifecycle |
| Model step / attempt | One provider request with captured settings, request ID, usage/routing provenance |
| Steering input | Independent input_id targeting one expected active turn_id; received/applied lifecycle |
| Attachment | Connection and observation resources; no execution ownership |
| Execution epoch | New each server start; fences workers/control/approvals, not durable object identity |

Provider-local tool IDs are qualified by model step internally; SDK results retain
the original IDs/metadata required by the protocol. Repeated IDs in later steps
must not collide. Reject duplicates within one complete response before any
call executes. Every started Item has a unique terminal record; an interrupted
assistant Item does not represent a complete message.

The following transitions are R0 review notation, not implemented Rust enums:

```text
Thread: idle -> busy -> idle                 (successful settled Turn)
        busy -> paused                      (failure/cancel, queue held)
        busy -> recovery_required           (unknown execution/effects)
        paused -> idle                      (explicit resume, blockers clear)
        recovery_required -> paused         (investigation confirms safe state)
        any -> closing                      (seal admission, clean up)
Turn:   queued -> running <-> waiting_for_approval
        running -> settling -> completed | failed
        queued -> cancelled                 (withdraw before activation)
        running/waiting -> cancelling -> settling -> cancelled
        active -> interrupted/recovery_required -> confirmed checkpoint
```

Queue pause and recovery blocking must remain separate facts even if the chosen
Thread enum combines their presentation. Waiting for approval, settling and
cleanup are not idle. Unknown process termination cannot release a workspace.
Targeted queue cancellation does not cancel active work or clear other inputs.

Preserve one active Turn per canonical workspace as well as per Thread. Shared
reads inside that Turn use a separate scheduling layer. Canonical-path exclusion
does not isolate overlapping roots, external editors or other host processes.

## 7. Input admission, queue and steering

| Operation | Required semantics |
| --- | --- |
| start | Admit/start only when Thread is idle, authorized and resources/lease available; otherwise reject with cause |
| enqueue | Durably admit a future Turn and return turn_id/order; no current-context mutation |
| steer | Atomically target expected active turn_id; admit input_id as received |
| cancel_turn | Target one active Turn; acknowledge intent, then stop/clean/settle |
| cancel_queued_turn | Withdraw one not-started Turn atomically against activation |
| resume_queue | Authorized explicit resume after checking blockers/resources; no replay of completed work |

Admission validates caller, workspace grant, input size and capacity. Commit
acceptance, deduplication and control state in one transaction before returning
accepted. Start obtains execution lease/capacity; enqueue obtains them on
activation. Rejections append no message or accepted-key record.

Deduplication is scoped to caller, durable Thread and operation; create-Thread
keys are caller/operation scoped. Server instance is not the only scope. Check
accepted keys before busy; same key/request returns original identity and current
state, conflicting reuse fails. Persist keys with retained objects. Expiry must
explicitly report that old execution cannot be confirmed, not treat it as new.
RPC correlation IDs are not acceptance keys.

FIFO entries enter model context only after durable activation. Start cannot
bypass accepted queued work; activation/control is serialized by the state owner.
Normally completed, cleaned and settled work advances the queue. Failure, cancel
or unknown effects pause it with a saved cause. Temporary capacity/lease shortage
waits visibly without dropping accepted input. Resume revalidates authorization
and does not restart an uncertain Turn from its original prompt.

Steering admission stops launches from the old response that have not started.
Let in-flight effects settle at a safe boundary; do not kill a modifying operation
by default. Settle committed but unstarted calls as not_executed_due_to_steer.
A model request may be preempted; partial output is display-only. If it cannot
stop promptly, its eventual response cannot launch stale tools after steering.

Persist received/applied or a reason for non-application. Applied binds context
version and the next model step. Receiving input is not proof the model saw it.
Apply multiple inputs in admission order, once; never reinject applied inputs
on recovery. A stale expected_turn_id is rejected, not moved to the next Turn.
Recheck admitted steering before verification completion/terminal commit/queue
advance. Cancel/unrecoverable failure records why pending steering was not used.
Steering cannot silently broaden permissions or permanent settings.

### R0 operation examples for review

These DTO sketches illustrate required fields, not released API routes/schema:

```json
{"op":"enqueue","thread_id":"th_A","idempotency_key":"input_2","prompt":"Fix the caller"}
{"accepted":true,"thread_id":"th_A","turn_id":"tu_2","state":"queued","queue_order":2}
{"op":"steer","thread_id":"th_A","expected_turn_id":"tu_1","idempotency_key":"correction_1","text":"Keep the public interface unchanged"}
{"accepted":true,"input_id":"in_1","turn_id":"tu_1","state":"received"}
{"event":"steer_applied","input_id":"in_1","turn_id":"tu_1","context_version":7,"model_step_id":"ms_4"}
```

The authenticated adapter supplies caller identity. Final names, epoch fields,
error DTOs and compatibility conversion must be frozen before client changes.

## 8. Turn execution and bounded scheduling

```text
commit activation/User Item
  -> capture step snapshot and validate prompt/bounds
  -> SDK model request, provisional assistant deltas
  -> validate full response/call IDs, reserve settlement capacity
  -> commit complete response and calls
  -> validate arguments/permissions, wait for needed approvals
  -> reserve workers/resources and commit execution intent
  -> execute within barriers; commit each result as it arrives
  -> settle all calls; construct SDK results in original call order
  -> apply steering; continue sampling or verify/clean/commit terminal
```

Reject incomplete/invalid/truncated responses before executing calls. Without
calls, validate finish reason and meaningful final output; empty or length-cut
answers do not constitute successful delivery. Preserve safe SDK fallback, not
whole-Turn retries after output or effects. Keep SDK cleanup/usage settlement
when cancelling a request; cancelled waiting does not establish zero cost.

| Tool class | Resource rule |
| --- | --- |
| read, glob, grep | Workspace-shared, bounded concurrency |
| write, edit | Workspace-exclusive |
| shell, verification | Workspace-exclusive, regardless of purported read-only command text |
| Future registered tool | Trusted metadata; exclusive if unspecified |

Preserve submission barriers: read A/read B may overlap, then edit A waits for
both, then read A observes the edit. Do not launch every call with join_all.
Reserve per-Thread/global workers and budgets before start. Approval waits hold
Thread/workspace execution lease but no running-tool permit or exclusive tool
lock. Approval permission and scheduling eligibility are separate checks.

State owner commits each completed result; completion order can affect progress,
not call/result pairing or model-message order. No next model request until all
committed calls are settled. On an error, cancellation or steering, stop new
launches and collect/clean up owned work. Ordinary tool errors/denials may be
fed back to the model after settlement rather than automatically failing the
Turn. R0 specifies error/denial classification and how unstarted calls are
settled; it cannot permit bypassing a failed commit. Store failure,
lost execution or critical unknown effects always block ordinary advancement.

Settlements distinguish completed, interrupted/effect-unknown and not-executed.
Never fabricate stdout, exit code or success. A result commit failure stops new
calls and model requests; in-flight work is stopped/collected, and unresolved
facts remain recovery blockers. Repairing message structure alone cannot prove
operation safety or authorize queue advancement.

## 9. Context, history and observation

Maintain two views from durable execution facts under explicit transitions:
SDK messages for model input, and complete Item history for clients. Not every
fact goes into the prompt. The evictable delta cache is authoritative for neither.

Reject orphaned calls/results. Preserve original provider ID/metadata and
step-specific association. Partial assistant streams remain interrupted display
records. Cancellation, bounds and steering settle every committed call before
building another prompt, including calls that never started.

Do not silently trim old history. Until compaction exists, stop with context_limit
and require a new Thread. Bound/truncate tool output with visible metadata.
Serialized bytes and provider token limits are different bounds; a 512 KiB
prompt limit does not guarantee provider capacity. Record source, step, context
version and available workspace version without a full evidence graph.
Reserve room for bounded results/errors/terminal state before calls are admitted.

Events carry server_instance_id, thread_id, monotonic sequence, timestamp and
relevant Turn/Item/input identities. Persist authoritative sequence progression
across restart; deltas need not be individually durable. Minimum control events
include queue admitted/cancelled/paused/resumed, steer received/applied/not-applied,
recovery blocked/resolved, Item lifecycle, approvals, cancellation and Turn end.

Atomically register observation with a snapshot cutoff; snapshot precedes newer
updates. Snapshot includes active Turn, queue/order/pause, steering, settings,
pending approvals, recovery blockers and bounded live output. Paged history
uses a fixed cutoff. Terminal records remain available after deltas expire.

Bound subscribers/caches by bytes and count. Slow clients never block execution
or approvals/cancel; resynchronize when necessary. Restart invalidates connections
and volatile deltas: reload durable snapshot/history, bind the new epoch, then
subscribe. Do not resubmit old work to reconstruct UI.

## 10. Tools, permissions and approvals

Registry owns name, schema, effects, resource requirements and handler, rejecting
duplicates. Model declarations and dispatch share it. Validate arguments before
permission decisions, using registered metadata, not model assertions. Hide
forbidden tools and reject forged dispatches. Keep current path checks/output
bounds/edit behavior; OS support needs platform-specific execution evidence.

| Profile | read / glob / grep | write / edit / shell / verification |
| --- | --- | --- |
| read_only | Allow within server restrictions | Deny |
| ask | Allow within server restrictions | Ask per invocation |
| allow_effects | Allow within server restrictions | Allow within server restrictions |

Thread stores effective profile. Hard restrictions override approvals. Proposed
creation defaults: native interactive/ACP ask; compatibility headless may request
allow_effects when authorized; existing --read-only maps to read_only. Attach
inherits policy; an observer cannot auto-approve another client's work.

Bind approval to Thread, Turn, Item, attempt, exact validated arguments, workspace
and current epoch. First authorized valid answer commits once. Recheck cancel,
steering, grants and target before launch. Allow once is only that invocation.

Persist pending and resolved approvals. After epoch/attempt change, stale replies
cannot launch effects. Restore necessary unstarted approvals with new current
identities; never replay completed effects because an approval was restored.
Disconnect is neither approval, rejection nor cancellation. Pending approvals
remain visible without a client. Only time with no active execution and pure
approval wait is excluded from Turn active duration; overlapping work still counts.

## 11. Durable commits, recovery and lifecycle

| Durable fact | Minimum contents |
| --- | --- |
| Thread | Identity/access/workspace, settings/permissions, context version |
| Inputs/control | Idempotent acceptance, FIFO order, steering target/application, cancellation and pause intent |
| Model step | Request/attempt, snapshot, full response/calls, available routing/usage |
| Tool | Identity, validated arguments, resource/approval, execution intent, result/uncertainty |
| Turn | Lifecycle, cumulative limits, verification, outcome, safe checkpoint |
| Observation | Bounded authoritative Item content/terminal facts and durable sequence |

No plaintext credentials or unlimited output storage. Durable artifacts may
hold large bounded content, with quota/retention/deletion defined in R0.

```text
acceptance transaction commits -> acknowledge accepted
full response/calls commit      -> permit scheduling
execution intent commits       -> permit effect
result commits                 -> permit model consumption
cleanup/context/end commits    -> publish terminal / advance queue
```

Queue insertion and deduplication commit atomically. A channel send, memory update,
queued write or flush is not necessarily a durable commit. R0 defines storage
synchronization and supported faults. MVP proves process crash/restart behavior;
do not extend this to arbitrary power loss or exactly-once effects.

Database transactions cannot atomically include file changes or arbitrary shell.
The effect-after-intent/before-result window remains uncertain even with a log.

| Recovered facts | Allowed action |
| --- | --- |
| Queued, unstarted | Rebuild FIFO; recheck grants/lease/capacity before activation |
| Result committed | Rebuild Item/context; do not execute that call again |
| Confirmed checkpoint, no in-flight effects | Continue same Turn with a new recorded step/attempt |
| Partial model stream | Interrupted display; new request if safe; retain unknown usage/cost |
| Execution intent, no confirmed result | Block; confirm old execution termination and investigate effects |
| Unapplied steering / pending approval | Restore target/control state; no duplicate injection or stale answer |
| Cancelled or paused queue | Preserve intent; restart does not automatically resume it |

Recovery checks three separate conditions: legal message history, execution
termination, and effect status. If old processes may still run, retain exclusion.
Critical unknown effects set recovery_required and pause the queue. Investigation
and explicit resolution are recorded operations; a new read is not a historical
result. R0 must define those operator actions before recovery is shippable.

File fingerprints/preimages may assist write/edit investigation; matching current
content alone does not prove a previous write completed. Arbitrary shell cannot
automatically replay. No effect rollback, cross-crash exactly-once or transparent
old PTY/process-session adoption is promised.

Acquire exclusive execution ownership before restarting work. A fresh epoch,
PID or in-memory lease alone is not proof old workers are gone. R0 chooses how
to confirm termination/isolation and fence store writes. If evidence is absent,
remain visibly blocked rather than release a conflicting execution. Keep durable
Thread/Turn IDs; reject stale-epoch approvals/control against new attempts.

Cancel/shutdown seal scheduling, cancel and join owned execution, then settle.
Unconfirmed cleanup remains a blocker with honest uncertainty. Restart can
continue confirmed checkpoints, not restart an unknown prompt wholesale.

### Failure window example

```text
edit intent committed -> file replaced -> process crashes before result commit
restart -> new epoch, same Thread/Turn/Item, recovery_required
        -> confirm no old execution; investigate recorded target/preimage
        -> record explicit evidence/resolution; settle context
        -> only then permit safe continuation / authorized queue resume
```

Receiving cancel or fabricating an error ToolResult does not resolve this window.

## 12. Verification, budget and capacity

Keep assistant answer, verification and stop reason separate. Configured checks
report passed/failed/denied/unavailable; only passed permits verified success.
No check means not_requested. Use origin=verification Tool Item through the same
permissions and workspace-exclusive scheduler. Client delivery includes the check
outcome, not only the earlier assistant message.

Verification failure ends the Turn with evidence and pauses queued work; later
Turns may fix it. No automatic repair loop. Include bounded verification evidence
in subsequent supported server-context messages; never invent a provider tool
result without a matching call. Pass is not proof of all user requirements.

Track estimated/actual/unknown costs with provenance and requested/available
actual route. Unknown usage is not zero and requested-model pricing is not
necessarily routed-model cost. Persist cumulative usage across restart. Define
model-step and tool-call bounds separately from the current mixed steps count.

Queue waiting and confirmed inactivity during downtime consume no active wall
time. Potentially running lost work is not free inactivity. Concurrent tools'
durations are not summed as Turn wall time. Pure approval-wait exclusion follows
§10, not a timer pause whenever any approval exists.

These are product v0.2 **proposed defaults for R0 review**, not shipped constants:

| Resource | Proposed default |
| --- | --- |
| Hot resident Threads | 32 per instance |
| Active Turns, including approval wait | 8 per instance |
| Running tools | 4 per Thread / 16 per instance |
| Waiting Turns | 32 per Thread |
| Individual user input | 64 KiB |
| Model steps | 32 per Turn; tool calls separately bounded |
| Active duration | 600 seconds, excluding only pure approval wait |
| Serialized prompt | 512 KiB, separately from token capacity |
| Hot context/Item metadata | 2 MiB per Thread, including settlement reserve |
| Total hot state | 64 MiB, including settlement reserve |
| Durable Turns/deduplication | Cover retained objects; disk quotas/retention fixed in R0 |
| Unattached idle hot state | May unload after 30 minutes; durable Thread is retained |

Bound pending steering, approvals, tools/output, artifacts, subscriptions and
buffers. Specify effective values/accounting in capabilities. Hot eviction must
not delete context, queue or keys. Active, approval-waiting or recovery-blocked
Threads are not ordinary idle eviction candidates. Explicit durable deletion
requires no owned execution and clear authorized semantics.

Reserve settlement space before work and refuse admission on storage exhaustion.
Physical disk failure/fullness can still prevent commits: block scheduling and
recover from last durable facts; do not claim a reservation guarantees all writes
under arbitrary failures. Never delete active records or bypass commit to proceed.

## 13. Clients, compatibility and ACP

All clients share create/load/get/list Thread, start/enqueue/steer, targeted cancel,
queue resume, answer approval, history and observation operations. Every access
checks trusted caller and workspace authority; durable identity is not a grant.
Inference authentication bypass and read-only control access do not grant agent
execution. HTTP cannot gain access to local-only workspaces by loading a Thread.

Local clients connect/start the same bro serve. Explicit remote failure never
falls back to local execution. Negotiate capabilities/version for new operations,
limits and retention. Stale execution epoch requires reload/rebind; retrying a
persisted accepted key returns its original identity after authorization.

One-shot task_id becomes a compatibility Thread/Turn projection, not a second
runtime loop. Preserve machine output until versioned migration. Existing
--task-id observation stays compatible. Proposed bro code --thread-id <id> and
bro task run --thread-id <id> <prompt>, plus input-control names/routes, require
R0 review. Creation-only options cannot mutate an attached Thread.
This document does not publish those proposed flags as usable CLI commands.

A proposed no-agent bro acp serve is a thin native stdio bridge; explicit external
harness selection retains its meaning. ACP session maps to durable Thread; each
connection binds current instance. session/new creates, session/load replays and
observes, session/prompt uses a verified Turn mapping. Native queue/steer are not
automatically ACP standard capabilities: return documented busy/limits where
unsupported rather than invent extensions.

Replay uses an atomic cutoff, then live updates, including native-origin work.
Expose pending approval/live state; overflow fails/resyncs rather than claiming
successful incomplete load. Stdout is protocol frames, stderr diagnostics.
EOF/bridge loss only detaches; explicit cancel targets observed current Turn.
Restart requires load/rebind, not automatic unknown prompt resubmission.

Server executes filesystem/shell; do not delegate native tools to client RPCs.
Reject unsupported client MCP registration without launching it. ACP permission
RPC is connection-local; underlying approval is durable runtime state. Route to
one eligible attachment; another authorized client may resolve it. Late/cross-
epoch replies cannot authorize. Do not equate a disconnected RPC with consent.

Named real ACP client/version must prove load/replay, updates and obsolete approval
UI when another client answers. No invented dismiss notification. Record limits
where the client cannot represent state. Native/HTTP and ACP have separate gates;
both are in delivery scope. First-release batching and selected clients are R0
release decisions, not evidence supplied by protocol mocks.

## 14. Migration phases and implementation evidence

| Phase | Deliverable | Exit gate |
| --- | --- | --- |
| R0 Contract/synchronization | Consistent product/spec; identities/transitions, DTOs, persistence/ownership, limits, compatibility | Reviewed concrete operations and fault examples; no unused framework |
| R1 Durable execution records | Item/step snapshot, full-response/result commit and common settlement in current one-shot path | Working one-shot; partial streams, pairing, store failure and crash records covered |
| R2 Bounded concurrency | Worker limits, shared/exclusive barriers, cancellation, result commits | Reads actually overlap; writes retain order; stopped/failed batches leave no worker |
| R3 Continuing Thread/input | Retained context, FIFO, steering, approvals, reconnect and durable keys | Dependent Turns; queue/steer/finish/cancel races; disconnect/resync |
| R4 Safe recovery | Rebuild input/context/budget; ownership and effect investigation; continue checkpoints | Fault injection across commits; no completed-effect replay; unknown effects block |
| R5 Client delivery | Native/HTTP controls, old task projection, inbound ACP load/approval | Separate clients and selected real ACP client pass distinct gates |
| R6 Release evidence | Stress, platform/shutdown, live provider, quotas and docs | All required cases evidenced, remaining limits recorded |

Follow this order rather than replacing every public task name at once. R1 adds
commit acknowledgements to the actual runner, not asynchronous logging alongside
unchanged execution. R2 adds responsive worker supervision; R3 admits new input
through that owner; R4 uses the same records to recover. Safe restart continuation
is not claimed during intermediate record-only phases. All four expanded abilities
are MVP completion requirements despite incremental delivery.

Keep intermediate paths runnable. Route compatibility consumers through the new
runner and remove superseded execution paths once migrated. Create a new runtime
implementation record when source work starts, recording pinned references,
changed paths, deliberate differences, tests and incomplete gates. Old tests are
baseline assets; assertions intentionally enforcing state loss or old call-ID
scope must change with their corresponding phase, not be counted as new proof.

CLI/config/harness wiring changes update docs/CLI.md, skills/bitrouter and affected
plugin manifests together. Product publishing docs belong in bitrouter-docs.
Source changes require all-feature tests, clippy and fmt per AGENTS.md; run doctests
separately if the test runner omits them. This documentation edit claims no new
runtime test results.

## 15. Acceptance and remaining review

First complete integrated milestone: dependent Turns in one Thread, real bounded
read concurrency and ordered writes, visible queue/steering, approval/cancel and
reconnect; after restart, history/pending work load and safe continuation does
not duplicate effects or send malformed model history.

- [ ] Racing start accepts one Turn; enqueue durably admits separately; rejected input never enters prompt.
- [ ] Lost admission reply/restart retry returns original identity; conflicting key reuse fails.
- [ ] Second Turn consumes first's context; step-local tool IDs can repeat without Item/approval collision.
- [ ] read A/read B overlap; edit A is a barrier; later read A observes its result.
- [ ] Result completion order preserves pairing; batch/store failures stop launches and collect owned workers.
- [ ] Queue activation/cancel races activate at most once; failures/cancel/unknown effects pause; resume never replays completed work.
- [ ] Steering during model, tools, approval and verification stops old unstarted calls; received/applied are distinct; stale targets reject.
- [ ] Crash before/after steering application preserves ordering without duplicate injection; every committed call settles honestly.
- [ ] Competing approval replies authorize once; stale epoch/attempt answer or cancellation cannot launch/harm new execution.
- [ ] Inject process loss after admission/activation, before/after response commit, after intent and after effect before result commit.
- [ ] File changed without committed result requires investigation; unknown shell is not automatically replayed or skipped as successful.
- [ ] Old workers/processes not confirmed stopped block conflicting work; history repair alone cannot unblock effects.
- [ ] Partial assistant streams are display-only; cumulative budgets survive restart; unknown usage is not zero.
- [ ] read_only rejects forged effects/verification; attach cannot broaden policy or silently approve.
- [ ] Output floods, slow subscribers and replay races resync without blocking controls or losing authoritative terminal facts.
- [ ] Disk exhaustion/commit failure stops work; unloading hot state preserves durable queue/context/keys; quotas include cleanup records.
- [ ] Verification absent is not_requested; failures retain answer/evidence and pause queue; clients show final verification outcome.
- [ ] Real provider proves tool concurrency, continuous context and input controls through SDK, with actual routing/usage provenance.
- [ ] Named real ACP client proves load, native-origin updates, shared approvals and documented protocol limits.
- [ ] One-shot tasks and explicit external harnesses retain compatibility; every effect has one execution owner.
- [ ] Shutdown joins supported platform work; uncertain cleanup persists as a blocker, not terminal success with released lease.

Use scripted providers for state/stream errors, real subprocesses for effects and
cleanup, independent clients for races, and process faults for commit/recovery
windows. Keep mock, local checks, live provider, hosted CI, OS and real ACP client
evidence separate. Process-crash proof does not establish arbitrary exactly-once.

Confirmed product scope: existing tools, bounded concurrency, queue/steer,
persistence/safe recovery, one active Turn/Thread, full response commit before
execution, fixed-model baseline and SDK routing ownership.

R0 must resolve details without deferring those confirmed capabilities:

1. Effective worker/queue/steering/approval/hot-state/disk bounds and accounting.
2. Schema/transactions/synchronization/fault promise; execution ownership and effect investigation/resolution actions.
3. Steering boundaries, explicit interrupt semantics and client received/applied presentation; error/denial classification and batch settlement.
4. Permission defaults, pure-wait active timing, restored approval identity and control fencing.
5. Queue-resume authority, checkpoint continuation after failure/cancel, history/key retention and deletion.
6. Concrete CLI/API DTOs/versioning, named ACP clients, release batching and platform evidence.

No documentation synchronization marks these items reviewed or implemented.
