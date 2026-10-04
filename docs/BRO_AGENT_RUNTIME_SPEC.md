# BRO standalone runtime contract

Updated: 2026-10-04. Source baseline:
[`65555b12`](https://github.com/bitrouter/bitrouter/commit/65555b126e8d14982b2a7c977b618d545cd8f5b7),
PR #945. This document describes the implemented standalone Thread/Turn runtime
and its safety requirements. It incorporates the Thread/Turn unification;
readers do not need to apply amendments from historical specifications.

## Document authority and scope

| Subject | Current authority |
| --- | --- |
| Identity, admission, scheduling, context, commits, ownership and recovery | This document |
| Tool arguments, outputs, interpreter selection and filesystem limits | [Six base tools contract](BRO_BASE_TOOLS_SPEC.md) |
| Native Conversation, input and durable Thread navigation | [Conversation UI contract](BRO_CONVERSATION_UI_SPEC.md) |
| Command names, flags and configuration | [CLI reference](CLI.md) and shipped [BitRouter skill](../skills/bitrouter/SKILL.md) |
| Source-specific validation and remaining gates | [Runtime acceptance](BRO_AGENT_RUNTIME_IMPLEMENTATION.md), [tool acceptance](BRO_BASE_TOOLS_ACCEPTANCE.md), [UI acceptance](BRO_CONVERSATION_UI_IMPLEMENTATION.md) |
| Future core/harness separation | [Migration handoff](BRO_AGENT_RUNTIME_HANDOFF.md); not implemented by this contract |

The tool and UI contracts specialize this runtime; they do not introduce another
scheduler, durable authority or permission policy. Explicit ACP sessions use
[shared ACP navigation](CODE_TUI_CODEX_NAVIGATION_SPEC.md). Deprecated designs
and phase histories are accessible through pinned Git snapshots, not additional
current contracts. Validation records describe the named source, not every later
commit. Full product acceptance is distinct from local runtime acceptance.

Scope includes the native model/tool loop, durable continuous conversations,
bounded concurrency, FIFO/steering, approvals, observation and proved-safe
checkpoint recovery. Native multi-agent scheduling, adaptive model/context
selection, inbound native ACP, core integration, OS sandboxing and operator
resolution of lost owners or unknown effects remain separate work.

## Ownership and identities

| Component | Responsibility |
| --- | --- |
| App host | Database backend, assembly, authenticated local/HTTP transport and terminal I/O |
| `ThreadService` | One admission, queue/control, worker, commit, settlement and observation authority |
| `ThreadRecord` | Caller/configuration/grants, retained context, FIFO and the unique Thread commit gate/version |
| `TurnRecord` | Owning Thread, cancellation, approvals, launch fence and execution state |
| `Agent` | Model/tool loop using supplied context, permissions and acknowledged facts |
| SDK | Routed provider execution, fallback, usage and settlement; native calls exclude its server tool loop |
| `ExecutionStore` | Transactional facts, acceptance keys, root index, version CAS and writer fencing |

Each Turn belongs to one durable Thread. Items and model/tool invocations retain
stable identities; provider-local call IDs are separate. `RunStatus` describes
the agent loop, while `TurnStatus` also accounts for verification and settlement.
The CLI word `task` denotes a one-shot client experience, not a separate stored
Task domain. There is no public Task submit, legacy root conversion or second
execution loop. SDK dependencies do not point back to orchestrator.

Public operations authenticate the caller and check the serving instance,
Thread ownership and current canonical workspace/profile grant. Knowing an ID,
local transport access or inference `skip_auth` cannot grant effect permission.
ReadOnly hides and rejects effectful tools and verification. Ask requires
identified approval; AllowEffects requires the corresponding server grant.
Approval cannot broaden a grant, and grants are rechecked on reload and control.

## Admission and controls

| Operation | Behavior |
| --- | --- |
| `create_thread` | Persist owner/workspace/configuration/grants without executing |
| `start_turn` | Accept only an idle Thread without prior accepted queued work |
| `enqueue_turn` | Persist FIFO input; acquire execution resources at activation |
| `steer` | Target the expected active Turn; apply once at a model boundary |
| `cancel_turn` / `cancel_queued_turn` | Cancel the originating active Turn or withdraw one unstarted input |
| `answer_thread_input` | Bind caller, epoch, Thread, Turn, approval ID and operation key |
| `resume_queue` | Explicitly resume a paused FIFO after rechecking blockers and capacity |
| View/history/observe/directory | Authorized public projections; no implicit execution or recovery |
| `load_thread` / `recover_thread` | Separate inspection/reload from explicitly accepted safe recovery |
| `unload_thread` | Release eligible resident resources without deleting durable state |

Acceptance keys are scoped to caller, Thread and operation; creation omits the
not-yet-existing Thread ID. Input, key and public changes commit atomically before
acceptance is acknowledged. Identical retries return the original identity or
receipt; changed input with the same key conflicts. RPC correlation IDs are not
acceptance keys. Rejection does not commit an input or key. An uncertain receipt
must be resolved with the original key and authoritative records, never a newly
generated submission. Unloading cannot delete accepted keys.

FIFO inputs enter model context only after activation. Start cannot bypass the
queue. Successful cleanup and settlement allow queue advancement; failure,
cancellation, storage uncertainty or unknown effects preserve a durable pause.
Temporary capacity/workspace contention waits visibly. Reload, reconnect and
inspection never replace explicit resume or replay a prompt.

Steering seals unstarted stale calls. Dispatched effects settle before the next
safe boundary; received steering is not proof it was applied. Old model responses
cannot launch stale calls after steering. Persist received/applied/non-applied
status, target, context version and step identity; apply accepted inputs in order
once, including after recovery. A stale target is rejected rather than moved to
a later Turn. Recheck steering before verification completion and terminal/queue
advancement. Cancellation records why remaining steering was not applied.

## Execution, context and verification

```text
commit activation and user Item
  -> validate step snapshot and budgets; commit model request
  -> obtain and validate complete response; commit response and calls
  -> validate tools/grants/approval; reserve resources; commit intent
  -> execute within ordered barriers; commit each result
  -> settle all calls in original model order; apply steering
  -> next model step, or verify, clean up and commit terminal state
```

Incomplete, malformed or truncated responses cannot launch tools or claim a
successful answer. Preserve SDK fallback/settlement, without whole-Turn replay
once output or effects have occurred. Partial model output is provisional display
evidence, not completed model context. Missing usage is unknown, not zero.

Read/glob/grep run with bounded shared workers. Write/edit/shell and independent
verification are workspace-exclusive. Ordered barriers preserve dependencies:
concurrent reads finish before a later edit, and a following read sees that edit.
Approval waits hold execution/workspace ownership but no running-tool permit.
Reserve per-Turn/global capacity and budgets before launch. Commit results as they
finish, but construct SDK call/result pairs in original call order. Another model
request requires every committed call to be settled.

Ordinary tool errors or denials may return to the model after settlement.
`EffectStatus` distinguishes known effects, unknown effects and NotExecuted;
an error alone does not establish that nothing executed. Failed result commits
seal new calls/model steps, collect owned work and retain recovery blockers.
Cancellation/shutdown stop launches and join cleanup; they cannot manufacture
stdout, success, a known result or a released workspace.

Model history uses complete validated instructions and paired responses/results
from execution facts. UI text, volatile deltas and evictable public history are
not model-context authority. Preserve provider metadata and legal pairing;
invalid/missing stable identities cannot be repaired by inventing replacements.
Bound context and response content before accepting work or effects.

Verification uses the same selected interpreter, grant, scheduler and workspace
as the Turn's tools. Keep assistant answer, verification and stop reason separate.
Statuses are passed/failed/denied/unavailable/not_requested; only passed establishes
the configured check succeeded. Failed verification ends the Turn and pauses FIFO;
there is no automatic repair loop. Subsequent context may include bounded check
evidence, without a fabricated provider tool result lacking its corresponding call.

Model steps, calls, spend and active duration have independent cumulative bounds.
Queue waiting and proved inactivity are not active execution time; potentially
running lost work cannot be treated as free downtime. Parallel tool durations are
not summed as wall time. Unknown usage/time/accounting remains explicit in recovery.
Requested model, actual route and estimated/reported cost retain their provenance.

## Durable facts and observation

```text
acceptance commit       -> acknowledge input
complete response commit -> permit tool scheduling
execution intent commit -> permit effect
result commit           -> permit context consumption
cleanup/end commit      -> publish terminal / advance FIFO
```

Database transactions cannot atomically include arbitrary shell or file effects.
An intent without a confirmed result remains uncertain. These process-crash
contracts do not promise power-loss durability, rollback, cross-crash exactly-once
effects or adoption of an old PTY/process session.

Execution facts and one public `ThreadEvent` commit in the same transaction.
Full model/tool/verification results do not undergo a second presentation-only
completion commit. Physical duplication between facts and public projection may
remain. Volatile text/shell deltas are bounded and do not advance the durable
cursor; committed entity IDs replace provisional content in place.

Views/history/receipts use bounded readers, pages and bytes at fixed cutoffs.
Gaps, malformed or oversized records fail visibly instead of silently omitting
Items. Observer registration and unload/admission coordinate atomically. Slow
observers resynchronize from a snapshot; expired history requires durable public
reconstruction. Detaching an observer never approves, cancels or advances work.
Historical event epochs remain historical; current serving envelopes use the
current epoch. Reconstruction consumes later committed writer epochs correctly.

## Store ownership and workspace exclusion

One active owner per execution store binds server instance and monotonic
generation. Claims accept an empty store or a durably stopped prior owner.
Active lost owners and unfenced facts block execution; elapsed time, PID loss or
a new epoch cannot retire them. Retired instance IDs cannot be reused. Commits
check owner/active state and version CAS in the same transaction as facts/keys.
Raw bootstrap/import writes are forbidden after ownership is established.

Startup discovery completes before writer authority permits admission. It scans
the durable root index with count/byte/record bounds and a fixed membership
cutoff. Failed/incomplete discovery cannot be bypassed by a cached owner.
Malformed/unsupported roots and unresolved cold executions block affected work;
complete discovery installs metadata, not hot contexts, workers or approvals.
Membership is fixed, while row/head status is a current projection. Inspection
without a writer fence never classifies old work safe for new execution.

Cooperating runtimes using different stores share an OS lock and a bounded,
atomically replaced marker outside the canonical workspace. The parent must be
writable; root paths without a parent are rejected. Lock files remain in place:
unlinking them could split exclusion. Missing/invalid/oversized markers and
symlink/nonregular coordination paths cannot authorize execution. A released
kernel lock does not prove effect cleanup; an active marker remains blocking.
This is local coordination, not OS isolation or distributed-filesystem fencing.

Before model requests and tool intents, validate the exact local launch identity
and active marker. Track blocking coordination I/O through shutdown. Known joined
work commits release preparation, writes the idle marker while holding the lock,
then commits final outcome before dropping guards and publishing terminal state.
Lost acknowledgements or uncertain cleanup withhold clean-stop proof; unknown
effects cannot prepare release. Cold inspection cannot overwrite or release an
unconfirmed active claim. Temporary external contention may retry eligible FIFO
activation; invalid or unconfirmed exclusion requires recovery, not ordinary resume.

Shutdown seals admission, cancels/joins workers, durably pauses queues and only
then records a stopped owner. A failed stop commit is not a release. The stopped
proof concerns tracked workers and writer retirement; it is not proof arbitrary
external processes cannot remain.

## Loading and safe recovery

Loading validates format, caller/grant, canonical workspace, stable identities,
context, durable cutoff and capacity using bounded scans outside global admission.
Installation rechecks current state under the unique Thread gate. Loading creates
no model/tool execution, approval sender or FIFO runner.

Same-instance reload of an eligible unloaded Thread may restore valid terminal
context under the same active owner and confirmed release. It preserves pause,
keys and identities. Optional absent usage for a settled historical Turn, when no
spend limit requires it, does not authorize cross-owner active continuation.
Cross-owner/instance inspection remains recovery_required until explicit recovery.
Pending approval metadata stays visible but old answers cannot authorize effects.

`recover_thread` is a Rust host operation, not a published CLI/HTTP operator
command. Its request binds inspected source epoch, exact cursor and an idempotency
key. Require current caller/grant, legal bounded context, workspace inspection,
a proved stopped source owner and complete writer discovery. Unknown effects,
unsettled calls/requests, uncertain accounting and invalid records remain blockers.
There is no TTL/PID takeover or operator-resolution implementation in this scope.

Recovery commits its original operation identity, recovery fact, checkpoint and
public event before runnable state is installed. Lost ACK adoption requires a
bounded reread matching the exact original batch and final cursor; uncertainty
stays blocked. Caller detachment does not cancel the owned operation. Identical
retries do not append another recovery or launch another worker.

Terminal recovery restores recorded idle/pause intent and keeps retained FIFO
paused until explicit resume. Active recovery requires a proved RunCheckpoint or
complete Settled outcome, original Turn/user Item and cumulative budgets. Validate
checkpoints against committed calls/results; completed tools are never replayed.
Fresh continuation steps get fresh step/Item IDs. Retained steering is applied
once to its original target. Exhausted budgets stop before a new request.

A complete settled answer or matching committed verification result is reused
without another model/check invocation. Verification association must match the
exact committed invocation. Partial output remains display-only with unknown
usage. Message pairing, an idle marker or a new epoch alone cannot prove safety.

## Capacity, unloading and format

`RuntimeLimits` in [service.rs](../crates/bitrouter-orchestrator/src/service.rs)
and `AgentConfig` in [agent.rs](../crates/bitrouter-orchestrator/src/agent.rs) define
shipped defaults; negotiated capabilities expose runtime limits. This document
does not maintain a second table of proposed numeric defaults.

Hot capacity is resident capacity, not lifetime conversation count. Unload only
an Idle or safely settled Paused Thread without queued work, active/cleaning
workers, pending approvals, operation references, observers, storage errors or
recovery blockers, and with confirmed checkpoint/release. Gate and admission
coordination prevent new work/observation between the check and removal. Unload
removes resident caches, not facts, keys, ownership, markers, cursor or pause.

Explicit unload and bounded LRU reclaim under pressure use the same conditions.
If all candidates are ineligible, return overloaded rather than steal a worker or
observer. Cold public queries do not load SDK context. Reload reserves reader/hot
capacity and cannot create a second gate for a Thread still referenced elsewhere.

Runtime root format is 2. Migration 000026 labels existing development roots 0;
unsupported roots fail before execution, without legacy conversion or deleting
facts/owner/markers. Format rejection cannot become stopped-owner proof. Developer
database cleanup is a separate authorized operation and must preserve unrelated
metering/configuration data and unresolved execution evidence.

## Client and transport boundary

Local protocol is **v15**; reject mismatches without fallback/resubmit. HTTP uses
**`/agent/v2`**, loopback, a dedicated bearer credential, workspace allowlist and
body/admission bounds. See [agent_api.rs](../apps/bitrouter/src/agent_api.rs) for
routes: create/read Thread, start/enqueue/read/cancel Turn, steer, approve, resume,
history and SSE observation. Directory listing is local-only, caller/grant filtered,
with fixed membership and bounded pages; the UI contract defines its navigation.
HTTP does not expose operator recovery or maintain its own execution queue.

`bro task run` creates/starts through the same service with stable separate keys.
An explicitly rejected start may leave an empty Thread; an uncertain receipt must
be resolved before treating it as empty. `bro code` retains a continuous Thread;
`bro code <agent>` remains external ACP. Reattach by Thread ID preserves stored
configuration and grants. Client detach/reconnect does not execute recovery or
resume FIFO. Native draft persistence is in-process, not across client restarts.

The native endpoint currently holds daemon admission for its serving lifetime;
ACP/HTTP idleness does not establish safe native epoch replacement. Automatic
native handoff remains deferred. Source-specific process/PTY/provider/platform
proof and remaining gates belong in the acceptance records, not historical specs.
