# BitRouter Orchestrator Core Specification

> Version: **1.0** · Date: **2026-10-01**
>
> Status: **Architecture and behavioral contract frozen for implementation.**
> This is a target specification, not a statement of shipped functionality.
> Changes to ownership, durable boundaries, or protocol behavior require a
> versioned amendment and corresponding acceptance changes. Private Rust names
> and module factoring may change without changing this contract.

## 1. Decision and scope

BitRouter provides a deployable orchestrator core that owns model routing,
context routing, and agent scheduling. A harness connects to that core, executes
workspace tools, stores durable workflow/session records, and supplies local
facts. The same core can run in process, on the local machine, or remotely.
Deployment location does not change ownership or authorization.

The public execution interface is Responses-shaped multi-agent execution with
an explicitly negotiated BitRouter extension. A separate routing-decision HTTP
request is not required before each inference. Ordinary model-compatible APIs
continue to work without activating managed orchestration.

One session has one active scheduling owner and one durable harness authority.
The core holds live execution state; the harness persists enough state to
reconstruct it. Core metering/continuation stores remain router-owned records;
they are not a second workflow recovery journal.

This contract applies to the first-party managed-core mode. Existing transparent
ACP integrations retain native harness ownership as described in
[ACP_CONTROLLER_SPEC.md](ACP_CONTROLLER_SPEC.md). An unmodified external ACP
agent is not automatically a thin tool-execution harness.

## 2. Ownership

| Responsibility | Core | Harness |
| --- | --- | --- |
| Agent graph and lifecycle | Create, reuse, message, wait, interrupt, cancel and schedule | Persist and display the resulting records |
| Model execution | Select model/effort, invoke the existing pipeline, validate compatibility, account for fallback and usage | Supply user constraints and execution feedback |
| Context | Select and assemble each agent's model input, manage revisions and handoffs | Store original material; discover and return local material |
| Workspace operations | Issue requests and consume confirmed outcomes | Validate permissions, obtain approval, lock resources, execute and clean up |
| Durable workflow/session | Define state transitions, checkpoints and replay semantics | Atomically persist and acknowledge them; serve recovery data |
| Local signals | Consume versioned facts and classify uncertainty | Report capabilities, workspace revision, execution status and resource facts |
| Observation | Produce attributed execution events | Render, paginate and retain durable history |
| Provider credentials | Host-configured router authentication | Never supplied as checkpoint content |

Harness tool resource scheduling is local enforcement, not a second agent
scheduler. A harness may reject or delay a tool request; it must not silently
change the target agent, model, task, or context plan. Core decisions cannot
expand harness permissions.

```mermaid
flowchart LR
    H[Harness: tools, durable store, local facts] -->|inputs, results, restore| C[Orchestrator core]
    C -->|events, commit batches, tool requests| H
    C --> R[Shared model routing pipeline]
    R --> P[Providers]
    P --> R
    R --> C
```

## 3. v1 deliverable

The complete v1 includes:

- Root and child agents, bounded concurrent model execution, reusable child
  contexts, messaging, follow-up work, waiting and cancellation.
- Model/effort selection using existing routing machinery, explicit overrides,
  and recorded actual execution configuration.
- Context decisions covering continuation, adding material, rebuilding a view,
  reusing an idle compatible agent, and creating a focused child.
- Client-executed workspace tools, durable commit acknowledgements, session
  restoration, uncertain-effect blocking and stale-owner fencing.
- A Responses-shaped execution adapter and a versioned harness channel, both
  consuming the same core operations as the in-process integration.
- Queueing and steering with explicit targets, bounded storage and cancellation.

Delivery is staged; a mock harness enables core development independently of
the production harness. Mock evidence alone does not complete v1 integration.

Deferred: learned context optimization, Jev integration, arbitrary evidence
graphs, automatic rollback, transparent process/PTY migration, multi-harness
workspace distribution within one session, online core migration, automatic
merge of concurrent workspace modifications, and an independent decision API.
Provider-hosted multi-agent forwarding is a separate capability, not the
implementation of BitRouter-owned scheduling.

## 4. Identities and state

| Object | Identity and meaning |
| --- | --- |
| Session | Harness-issued `session_id`, scoped to the authenticated owner; durable conversation and agent tree |
| Run | `run_id`; one accepted root task including its descendant work; at most one active root run per session |
| Agent | Core-issued stable `agent_id`; parent link and display path; contexts may survive across runs |
| Agent turn | `agent_turn_id`; one assigned task/follow-up; at most one active turn per agent |
| Model step | `step_id`; one immutable model-input snapshot; provider attempts have separate `attempt_id` values |
| Response | `response_id`; one client-facing Responses exchange; a run may span several responses |
| Operation | `operation_id`; idempotent client input, tool result or control operation |
| Tool invocation | `invocation_id`; durable execution identity independent of provider-local `call_id` |
| Decision | `decision_id`; a routing plan and its application/outcome correlation |
| Ownership | `execution_epoch` plus `core_instance_id`; current scheduling grant |

An existing native `Thread` maps to Session, and its root `Turn` maps to Run.
Response exchanges and child agent turns must not be collapsed into that root
Turn. Core allocates run/turn/step IDs before proposing their acceptance and
retains those IDs on retry.

Core emits `state_revision` for committed scheduling state, per-agent
`context_revision`, and session-wide monotonically increasing `event_seq`.
Harness-owned `workspace_revision` and `signal_revision` are distinct. Unknown
workspace versions remain unknown; a Git commit alone does not describe dirty
files. Provider call IDs are scoped by step and agent. The adapter maps them to
unique public call IDs and durable invocation IDs without rewriting historical
provider records.

Run states: `queued`, `running`, `waiting`, `cancelling`, `recovery_required`,
`completed`, `failed`, `cancelled`. Agent-turn states distinguish `runnable`,
`model_running`, `waiting_tool`, `waiting_message`, `waiting_commit`,
`interrupted`, and terminal outcomes. Commit/connection blocking is orthogonal
to the last committed execution state; losing a connection does not complete
or cancel a run.

A root final answer is provisional while owned descendants or effects remain
unsettled. Completion requires joined/cancelled descendants, tool cleanup,
configured verification, and a committed terminal checkpoint. Agent final
messages, verification outcomes and run terminal reasons remain separate.

## 5. API and compatibility boundary

### 5.1 Entry points

- Existing `/v1/responses`, `/v1/chat/completions`, `/v1/messages` and other
  ordinary inference routes retain their behavior without managed opt-in.
- Managed execution uses `POST /v1/responses`, `multi_agent.enabled: true`, and
  `BitRouter-Beta: orchestrator_core=v1`. The `bitrouter` request extension below
  is required. It is consumed locally and never forwarded to a provider.
- `GET /v1/orchestrator/capabilities` reports protocol version, supported
  operations, transport modes, limits and unsupported compatibility features.
- `GET /v1/orchestrator/channel` upgrades to an authenticated WebSocket carrying
  the v1 harness protocol. One connection may bind one session in v1.
- The in-process harness uses the same operation types and commit boundaries.
  It does not bypass acknowledgement merely because it shares a process.

These are target interfaces. The existing read-only administration API is not
repurposed to accept execution commands.

Managed mode consumes `multi_agent` locally as well as the BitRouter extension.
It does not forward a flag that starts an upstream provider-owned agent tree.
Models receive core-owned collaboration function declarations; their calls are
intercepted by core and never also dispatched to the harness or SDK server-tool
loop. Provider-hosted orchestration requires a separate explicit execution mode.

```json
{
  "model": "bitrouter/coding",
  "input": "Investigate the failing login test and produce a verified fix.",
  "stream": true,
  "multi_agent": { "enabled": true, "max_concurrent_subagents": 3 },
  "bitrouter": {
    "version": 1,
    "execution": "managed",
    "session_id": "session_01",
    "execution_epoch": 1,
    "operation_id": "input_01",
    "routing": { "model": "policy", "context": "auto" }
  }
}
```

The harness must bind the session and confirm its initial checkpoint before
this request can start model execution. IDs are references, not authorization.
The HTTP request and channel binding must resolve to the same authenticated
owner. Unknown extension versions and unsupported required features fail
explicitly before execution; no silent fallback to another execution owner.

Model mode is `fixed` or `policy`; context mode is `fixed` or `auto`. Missing
routing settings use `fixed`/`fixed`. Fixed context still permits explicit
collaboration commands; it disables unsolicited context restructuring.
Explicit provider/model and effort constraints remain authoritative. A child
inherits effective settings unless an authorized override narrows or changes
them. Per-agent routing is a BitRouter extension.

### 5.2 Responses projection

Reference checked on 2026-10-01: [OpenAI Responses multi-agent guide](https://developers.openai.com/api/docs/guides/responses-multi-agent).
It separates hosted collaboration actions from client function execution,
attributes agent output, and supports continuation across response exchanges.
Its documented agents share request model/tools; its encrypted inter-agent
items are provider-specific. These facts are the reference boundary, not a
claim that this repository implements that beta.

The v1 adapter exposes ordinary message/function-call/output shapes and agent
attribution. Collaboration events use `bitrouter.*` extension events until
native `multi_agent_call`, `multi_agent_call_output` and `agent_message` fidelity
has explicit conformance tests. It must not manufacture OpenAI encrypted items
or advertise complete OpenAI beta compatibility. Opaque upstream state cannot
be decoded or moved across providers by renaming its fields.

For the HTTP/SSE v1 transport, an exchange reaches its terminal response once
all runnable agents have finished or are waiting for externally supplied tool
results/control. The exchange terminal is committed before success delivery;
`response.completed` does not by itself mean `run.completed`. The terminal
extension states `run_id`, run status and pending invocations. A continuation
request submits outstanding function outputs and references the core-owned
response ID. The core validates an exact pending-call mapping before resuming.

Function outputs can alternatively arrive over the harness channel. HTTP and
channel results normalize to the same idempotent operation; sending both does
not execute or consume a result twice. Resuming a terminal HTTP response opens
a new response; that response is never reopened. Active-response
`response.inject` compatibility is deferred and must be advertised unsupported.
Tool batches may run concurrently at the harness after the HTTP exchange ends;
streamed call fragments never authorize execution.

For this HTTP profile, core sends `tool.execute` only after the exchange's
tool-wait terminal checkpoint is committed. The harness can then execute the
authorized batch regardless of whether the UI has consumed the final SSE
frame. A duplicate continuation for the same input operation attaches to its
existing response/run disposition rather than starting another exchange.

Core response IDs and provider continuation IDs are separate namespaces. A
core response ID resolves to a session checkpoint, not directly to one upstream
`previous_response_id`. Stateful continuation requires this harness binding;
v1 does not promise restoration from response IDs alone or full stateless
multi-agent transcript replay by an arbitrary OpenAI client.

The preview HTTP adapter requires each continuation result to carry its durable
operation identity and full outcome metadata. For example:

```json
{
  "model": "bitrouter/coding",
  "previous_response_id": "resp_previous",
  "input": [{
    "type": "function_call_output",
    "call_id": "call_public",
    "output": "Verified tool result",
    "bitrouter": { "operation_id": "result_01", "status": "succeeded" }
  }],
  "multi_agent": { "enabled": true },
  "bitrouter": {
    "version": 1, "execution": "managed", "session_id": "session_01",
    "execution_epoch": 1, "operation_id": "continue_01"
  }
}
```

Optional result metadata includes `evidence` and `workspace_revision`; `status`
uses the same `ToolOutcome` values as channel results. When delivering through
both transports, use `result_01` and the exact same metadata for `tool.result`.
The successor and new result receipts share one checkpoint. Initial task
verification, acceptance criteria, material requirements and discardable history
are inherited and must be omitted on continuation. Model, effort, output-token
limit, routing and descendant limit cannot change the frozen run settings.
SSE currently projects retained output after the durable exchange boundary;
live provider-token forwarding is not part of the preview's conformance claim.

### 5.3 Input and control

`start` rejects a busy session; `enqueue` durably accepts a future root run;
`steer` targets an active run/agent turn and has distinct received/applied
records. Queue advancement happens only after successful root completion and
cleanup. Failure, cancellation or uncertain effects pause automatic advancement.
`resume_queue` is explicit. Steering never changes an in-flight step snapshot;
it blocks conflicting unstarted work and is applied at the next safe boundary.

The channel carries `session.bind`, `session.restore`, `input.enqueue`,
`input.steer`, `run.cancel`, `agent.cancel`, `queue.resume`, `signals.update`,
`tool.result`, `tool.status`, `material.result`, `checkpoint.ack`,
`session.head`, `operation.get`, and `session.release`.
Root `start` is the managed Responses request. Tool approval is a harness
operation; the core observes waiting, denial and execution outcomes.

Core outbound messages include `checkpoint.proposed`, `tool.execute`,
`tool.cancel`, `material.request`, `artifact.put`, and typed operation/query
receipts. `session.head` reconciles the core-known and harness-durable heads;
`operation.get` queries a retained operation's disposition and assigned IDs.
`session.release` requires settled execution, commits the release, and prevents
new dispatch by that instance; releasing is not an implicit successful cancel.

## 6. Harness protocol and durable commits

### 6.1 Envelopes and payloads

Harness-to-core messages contain `version`, `type`, `session_id`,
`execution_epoch`, `operation_id`, and a typed `payload`. Mutations carry
`expected_state_revision` where they change scheduling intent. A tool result
instead binds its immutable invocation/attempt and need not match the latest
unrelated state revision. Duplicate operation IDs with identical content return
the original disposition; different content under one ID is a conflict.

Core-to-harness durable output is a `checkpoint.proposed` batch containing:

```text
Envelope: batch_id, session_id, execution_epoch, core_instance_id
          payload_encoding = "base64-json", payload_bytes, payload_sha256
Decoded payload: matching envelope identities, base_event_seq, base_state_revision
                 events[]: { event_seq, type, run_id?, agent_id?, payload }
                 checkpoint: { schema_version, state_revision, artifact_refs[], state }
```

The digest is SHA-256 over the decoded UTF-8 payload bytes, serialized once by
core and retained for retries. `payload_bytes` is their standard base64 encoding.
The transport must carry those bytes, not recompute a digest from reordered
JSON. All envelope identity fields are repeated inside the hashed payload and
must match before admission. One batch is
outstanding per session in v1. The harness atomically persists the events,
checkpoint, operation deduplication facts, and all referenced durable artifacts
before acknowledging `(batch_id, payload_sha256, through_event_seq,
state_revision, execution_epoch)`. The append compares the stored base sequence
and revision. Exact retransmission returns the prior acknowledgement; a
conflicting append, missing artifact or epoch mismatch is rejected.

Checkpoint payloads also carry `tool_start_fences`, a list of exact
`invocation_id` / `attempt_id` pairs scoped by the batch's session. On steering
receipt, core includes every unresolved workspace invocation of the target turn.
The proposal recording a run cancellation or subtree interruption includes the
unfinished invocations of every newly cancelled turn, including unknown effects.
Root failure applies the same barrier to its descendants. Restoration reasserts
fences for retained cancellation before its acknowledgement; it does not infer
an outcome for uncertain work. These are the same committed-cancel guarantees
required in section 7, with no new wire fields or limit increases.
The harness atomically persists these tombstones with the append and head,
serialized against its local execution-start admission. Receiving `tool.execute`
or creating a pending approval is not a start: the start boundary is durable local
intent after approval and policy checks, immediately before allowing effects.
If the fence wins that order, neither a delayed execute nor an old approval may
start the invocation. If start wins, the tool continues and reports its actual
outcome; the fence cannot roll back an effect or certify `not_executed`.

Fences are retained even for identities absent from the harness execution ledger,
and survive batch replay, restart, epoch change and log compaction. They cannot be
removed when steering is applied or cancelled. A harness must durably settle a
fenced unstarted invocation as `not_executed` and deliver `tool.result` using the
original identities; core waits for that result before applying steering. An ACK
alone is not a tool result. A peer unable to enforce this barrier must reject the
checkpoint, including the initial binding proposal, rather than ignore the field.
Legacy persisted payloads without fences remain decodable for restoration; all
new proposals explicitly carry the field. This completes the existing v1
unstarted-work barrier, rather than changing tool approval ownership.

Only the matching acknowledgement advances core's committed state. Before
then the transition is tentative; dependent dispatch and terminal publication
are blocked. Final answers, accepted-input confirmations and collaboration
success receipts are durable events. Text/progress deltas may be ephemeral,
bounded and dropped; they are never a source of recovery truth.

Minimum payload contracts:

| Message | Required content beyond envelope |
| --- | --- |
| `session.bind` | Harness identity, core-instance grant, epoch, durable head/checkpoint, capability manifest, effective constraints |
| `session.restore` | Bound grant, checkpoint and journal tail, retained operation identities, pending-tool reconciliation, artifact availability |
| `session.head` / `operation.get` | Query ID and requested head/operation; return committed revision/sequence/epoch or retained operation disposition, with unknown distinguished from rejected |
| `signals.update` | Monotonic harness signal revision; timestamp, scope, source and values; explicit unknowns |
| `tool.status` | Invocation and attempt, status (`not_started`, `waiting_approval`, `running`, `stopped`, `effect_unknown`), evidence refs |
| `tool.result` | Invocation and attempt, terminal status (`succeeded`, `failed`, `denied`, `not_executed`, `effect_unknown`), output/evidence refs, observed workspace revision |
| `input.steer` | Target run/turn, input identity, content and expected revision |
| `run.cancel` / `agent.cancel` | Exact target, expected revision, cancellation identity |
| `checkpoint.ack` | Exact batch identity and durable head described above |
| `material.result` | Material request ID, immutable reference, version/digest, media type, bounded content or unavailable reason |

Harness reports are authoritative for its local execution records, but are not
proof of task quality or authority to exceed server limits. User/tool content
cannot become a control message through prompt text.

The harness maintains its tool-execution ledger separately from the ordered
core event sequence. Local tool intent/results survive a core disconnection;
they enter a core checkpoint through reconciliation, without the harness
inventing core events. Operation receipts are retained for at least the life of
their session/run and unresolved effects.

`material.request` identifies permitted material and expected version. Any new
filesystem/shell operation needed to produce material follows the tool path;
material retrieval cannot bypass approval. `artifact.put` transfers immutable
core-generated content in bounded chunks with artifact ID, digest, offset and
total length. Only a complete verified durable artifact may be referenced by a
committed checkpoint. Retries are idempotent; incomplete staging is never a
recoverable artifact. The local port obeys the same availability rules.

Staging failure or loss of its completion signal does not authorize dropping a
retained pending checkpoint or generating new execution identities. When the
original core session handle survives, retain it for exact head reconciliation.
Same-owner reconnect can stage
the same immutable archive again and retransmit the original pending batch;
an already durable batch is adopted by its exact head. Existing committed
roots and their dependencies remain available throughout replacement staging.
Actual owner/process loss follows the takeover rules below: revoke the old
authority, establish quiescence and restore under a new grant from durable state.

The local `HarnessPort::read_artifact(reference, offset, max_bytes)` returns a
bounded range of a complete immutable object. It is a required port method.
Before returning bytes or acknowledging a checkpoint referencing an archive
root, the harness verifies and retains the root's transitive dependencies. The
availability inventory can name direct roots; it need not flatten the complete
archive graph into every restore request. A missing root or dependency blocks
recovery. Content-addressed retries preserve the same bytes and identity.

Recovery histories may be represented by a `recovery_archive` reference in the
wire snapshot. Core hydrates and validates those records before using their
activity, lifecycle, uncertainty or approval facts. `CoreSession::snapshot()`
returns the complete hydrated state; directly decoding a wire checkpoint is
not a substitute for authenticated restoration. Artifact reads and staging
perform no workspace execution and run outside the session state mutex.

### 6.2 Required barriers

| Before | Durable record required |
| --- | --- |
| Confirming accepted input | Input, operation ID and queue/run identity |
| Starting a model attempt | Decision, immutable step snapshot, attempt intent and budget reservation |
| Executing any action from a model response | Complete validated output, calls and attribution; no partial call execution |
| Issuing a workspace execution command | Complete call plus pending invocation intent and frozen arguments/constraints |
| Starting the workspace tool | Harness-local execution intent/approval and exact dispatch authorization |
| Using a tool result or child message | Durable result/message plus its core consumption transition |
| Reporting a completed run | Descendant/tool settlement, verification and terminal checkpoint |

After the proposal containing a tool intent is acknowledged, core issues
`tool.execute` with `invocation_id`, `attempt_id`, `agent_id`, step/context
revisions, arguments, tool-manifest digest, workspace target, epoch and the
committed authorizing sequence. The harness validates this command and its
local policy before starting. Persisting or seeing a proposed call alone must
not cause execution. Execution-command retries preserve invocation identity.

Each new `tool.execute` includes frozen `result_limits`: `output_bytes` bounds
the UTF-8 result body, and `payload_bytes` bounds the complete serialized JSON
`ToolResult` or `ToolObservation`, including escaping, artifact metadata and
workspace revision. The latter leaves room within the run's `input_bytes` for
the largest control envelope. Both local and remote harnesses obey these
limits, including verification and restore reconciliation. A host/manifest
change cannot enlarge a previously admitted reply. Oversized evidence is
rejected before a checkpoint; it is never truncated or treated as a tool
outcome. These per-message limits are distinct from reserving space for all
outstanding results and terminal records in a checkpoint. `artifact_bytes`
bounds the sum of distinct immutable artifact body sizes referenced by each
result or observation. Repeated identical references count once; conflicting
references for the same ID and overflowing byte totals reject the message.
Missing `artifact_bytes` preserves a legacy execution contract: restoration
does not narrow an already authorized reply or invent a retroactive reservation.
Its evidence still receives payload, reference, availability and aggregate
quota checks. New invocations, including verification, always carry the bound.

The checkpoint contains the agent tree, turns, runnable/waiting states,
mailboxes, ordered context manifests and histories, pending calls, unconsumed
results, queue/steering/cancel state, decisions, budget reservations/consumption,
policy identities, response mappings and operation receipts. Large immutable
content may use durable artifact references. Provider-private items remain
origin-bound; credentials are excluded. A missing required object blocks
restoration rather than producing an incomplete reconstructed prompt.

### 6.3 Ownership, disconnect and recovery

The durable harness authority serializes ownership grants and monotonically
increments the execution epoch. A grant is bound through authenticated session
binding to one core instance; an arbitrary client header is not a grant. Only
the current grant may append, dispatch or consume new results. Same-owner
reconnection resumes the existing epoch after head reconciliation. Takeover
requires explicit revocation/reconciliation and a new grant.

Epoch fencing prevents stale commits and workspace execution; it cannot recall
an already sent provider request. Takeover must stop/fence the old scheduler
and reconcile in-flight attempts before dispatching replacement work. If this
cannot be established, remain `recovery_required`; v1 has no automatic online
failover. Late provider completion is recorded as evidence/usage without
granting the old instance scheduling authority.

On harness disconnect, core starts no new model attempts, child turns or
workspace dispatch. It requests cancellation of in-flight provider work and
retains bounded pending output for reconciliation. Work already running at the
harness follows its recorded cancellation policy. A dropped UI observer is
different from losing the durable harness. No success is reported for an
unacknowledged checkpoint. Since the harness is the durable authority, v1 does
not promise unattended progress while that authority is offline.

Recovery rules:

- Reconstruct from the last committed checkpoint and validated journal tail.
  Adopt a batch already committed at the harness even if its acknowledgement
  was lost; never append it again under new identities.
- Pending tools with committed results are not rerun. A known unstarted
  invocation may be redispatched with the same identity under the new grant.
- An execution intent without a confirmed outcome is reconciled at the harness.
  Unknown shell/write effects block dependent work; a synthetic error does not
  establish that the operation never happened.
  Explicit authenticated restoration can confirm an identical retained live
  definite result after missing or stopped evidence introduced uncertainty,
  without requiring a historical `effect_unknown` observation. An unknown
  result or another unresolved invocation still blocks dependent work.
- A model attempt without committed complete output is interrupted/uncertain.
  No partial tool calls are used. A later retry is a new attempt with previous
  unknown spend retained, not a replay of a known successful execution.
- Cancellation, queue pause, budgets and pending approval survive restart.
  Old approval identities do not authorize new execution attempts.
- A nonterminal run requires an authenticated cumulative active-time handoff
  bound to its run identity and exact restored durable head. The measurement
  includes the union of work through entry to the replacement core, including
  uncheckpointed work and process downtime, and excludes idle intervals. Unknown
  coverage blocks restoration. Per-attempt durations and disconnect duration
  cannot substitute for this measurement. Running tools then continue on the
  replacement's monotonic clock, including restore validation and ACK waits.
- Process-crash recovery is required. Cross-system exactly-once side effects,
  arbitrary power-loss durability and replayable private provider reasoning
  are not promised.

## 7. Agent scheduling and collaboration

All input sources enter one dispatcher: model collaboration calls, explicit
client controls, and policy actions at safe boundaries. The core owns their
state transitions; the harness persists them. SDK provider pipelines must not
also execute core-owned collaboration tools.

| Operation | Semantics |
| --- | --- |
| `spawn_agent` | Create a new agent and initial task; never silently reuse an existing one |
| `delegate_task` | Supply goal, acceptance criteria, context references and hard constraints; policy may choose a compatible idle worker or a new child |
| `send_message` | Durably enqueue a message; do not start a turn |
| `followup_task` | Assign work to an existing non-root agent; start when idle or enqueue behind its active turn |
| `wait_agent` | Wait for relevant mailbox/state changes; release the model-execution slot |
| `interrupt_agent` | Stop the active turn, preserve context and settle in-flight work; do not delete the agent |
| `list_agents` | Return authorized agents, tasks and committed status |

An agent's model-visible call is intent, not a permission grant. Explicit target,
fresh-context, isolation and model constraints are preserved. A task requesting
an independent review cannot be routed to a context whose reuse violates that
constraint. Runtime-originated actions are recorded as runtime actions, not
fabricated model calls.

Use deterministic FIFO within an agent and fair selection across runnable
agents. Descendant creation reserves tree capacity before acceptance. Waiting
agents release model slots but continue to count toward tree/context limits.
The root has a runnable opportunity when children would otherwise occupy all
model slots. Explicit wait dependencies must reject cycles; an all-waiting run
with no pending external input becomes a visible blocked condition, not a spin
loop. Mailbox consumption is committed once and applied only at safe boundaries.

Run cancellation propagates to descendants and tools. Agent cancellation
propagates through its subtree. No new work starts after a committed cancel;
observed cancellation provisionally blocks dispatch while its commit is
pending. A terminal cancellation waits for cleanup or records
`recovery_required`. Child failure is delivered as evidence; the parent may
repair/replan within its remaining limits. v1 has no detached child work after
root completion.

Provisional cancellation or permission revocation remains a dispatch barrier
even if another checkpoint is outstanding. It must be serialized after the
resolved batch before work can resume. This permits bounded asynchronous I/O
without holding a scheduler lock across a provider call or commit wait.

## 8. Joint context and model routing

Core evaluates a concrete work allocation:

```text
work intent + committed state + harness facts
  -> candidate executor/context plans
  -> hard feasibility checks
  -> policy selection of context + model/effort
  -> immutable step snapshot and durable decision
  -> shared model pipeline
  -> actual outcome and usage
```

The harness supplies facts and available material, not a compulsory preselected
agent graph. Core generates candidates, using the current agent and known
workers. First-party model-generated task descriptions or summaries are model
work: their cost and latency are recorded. Optional classifiers and learning
are not necessary for this path.

### 8.1 Context manifest

Each candidate contains an ordered manifest with `context_id`, revision,
agent/task identity, required constraints, selected evidence/history refs,
instruction/skill versions, tool-manifest digest, workspace revision and size
estimates. Each material reference includes provenance, content version/digest,
availability and whether it is required. A reference is not the material;
missing content is requested from the harness and verified before dispatch.
Core never reads the harness filesystem directly in managed remote mode.

Applicable user instructions, permissions and acceptance criteria cannot be
removed by relevance ranking. Tool-call/result pairs remain structurally valid.
Unverified agent conclusions retain their provenance. A toolset is frozen per
model step; removal affects future steps and does not retroactively reinterpret
calls already emitted. A permission revocation still prevents an unstarted
effect from executing.

Automatic decisions occur before model steps, after committed tool batches,
on task/phase boundaries, child results, steering, failure or changed resource
facts. No automatic context rewrite occurs mid-generation. Staying with the
current valid context is a first-class candidate. A stale candidate is rebuilt
or rejected, not silently applied to a later state.

### 8.2 Policy and feasibility

v1 uses a deterministic rule policy and explicit overrides. It must support
both continuing and at least one nontrivial context/delegation choice; merely
passing a fixed model does not satisfy context-routing acceptance. Reuse is
eligible only when task constraints, context versions, permissions and worker
availability agree. New isolated context is eligible when required material and
resources are available. Ambiguous reuse falls back to a feasible fresh
candidate or returns a blocked reason. Choice reasons are recorded.

The initial rule order is explicit: honor an exact target/fresh-context
requirement; otherwise continue an active work unit with valid context; for a
bounded delegation intent, prefer an idle eligible agent with an exact recorded
task-scope match, then a feasible new child. Stable agent ID breaks equal reuse
ties. An explicit spawn always creates a new agent. A stage/pressure signal
without a concrete child task does not by itself spawn agents. When continuation
is infeasible, build a candidate from versioned required material, or return
`no_feasible_route` if that cannot be done. A task-scope label is a matching hint,
not evidence that permissions, freshness or independent-review constraints hold.

Check actual prompt capabilities, provider protocol, context capacity including
output allowance, tool availability, continuation compatibility and remaining
resources. Byte limits are separate from token limits. Unknown token capacity
must not be presented as verified fit. An infeasible candidate is rejected;
required context is not silently truncated. Summarization/rebuild work is
explicit and counted, with its result validated before execution.

A transport declaring no request-level output cap can be admitted when the
explicit reservation covers its known, positive model output ceiling. The
ordinary maximum-output and combined-context checks still apply. The final
wire check permits only an absent cap for such a transport; silently rewritten
caps or unknown model ceilings remain failures. The decision retains both the
configured ceiling and the transport's lack of request-level cap support.

Settled work is not implicitly disposable. The initial deterministic history
reduction accepts an optional task-scoped `discardable_history` constraint from
the authenticated caller: `history_sha256` commits to the serialized canonical
history before that task, and `message_indices` identifies a strictly ordered
subset of old assistant/tool messages which the caller declares unnecessary for
this task. Omission retains everything. Instructions, required material, current
work and call/result integrity remain mandatory. This declaration is never
inherited by child assignments; a material inventory alone does not establish
that it replaces prior evidence. Reconstructed input must pass the frozen request
checks and joint feasibility checks before any generation. Preparation-added
messages need a separate dependency contract; the initial strategy rejects them.
A durable reconstruction candidate does not replace visible agent history until
read-only validation allows it under current source and dispatch gates and its
activation checkpoint is acknowledged. Unactivated candidates cannot be inherited
by children or later tasks. Validation has acknowledged intent/outcome barriers;
unknown hook/transform contracts reject it, request checks precede route guards,
and model selection and state registration are never repeated.

Model selection reuses the existing named router/policy machinery. A chosen
model/effort must not be independently selected again after the plan is frozen.
Execution revalidates hard constraints and records any fallback/continuation
adjustment as actual serving behavior. Manual model/effort overrides and
provider-native continuation restrictions take precedence over optimization.

If context reconstruction changes the required capabilities or capacity after
selection, revalidate the joint plan before dispatch. Provider-private
continuation, visible context reuse and prompt-cache reuse remain separate.
Switching models does not transfer KV caches. Unknown cache usage/cost is not
reported as an observed zero or guaranteed saving.

### 8.3 Decision and outcome records

`RoutingDecision` records decision/policy identity, source, input state/context
versions, candidate identities, selected executor/context/model/effort,
constraints, reason codes and estimate provenance. `DecisionApplied` binds the
decision to a turn and step, or records stale/rejected disposition.
`ExecutionReceipt` records actual attempts, provider/model/effort, continuation
adjustments, usage/cost provenance, elapsed time and outcome. Join all three by
decision ID; child quality and root acceptance remain separately attributable.

## 9. Harness signals and tools

Required signal groups are workspace identity/revision (including unknown),
tool schemas and trusted execution metadata, instruction/material inventory,
effective permissions, local resource availability and observed invocation
status. Optional signals include test outcomes, environment failures, user
preferences, context estimates and locally discovered evidence. Every update
is scoped and revisioned; prompt-inferred workflow labels remain distinguishable
from harness-observed facts.

The server intersects declared capabilities with host policy. Harness signals
do not replace API authentication or authorize cross-session access. Context
and result references are resolved only within the authenticated session's
permitted scope.

Credential-bound hosts recheck current dispatch authority after durable
admission waits and after transport queue/capacity waits. An acknowledged
intent does not extend an expired or revoked credential. Failed authorization
fences new dispatch while retaining accepted operation identities and evidence
for reconciliation; it cannot undo a provider request or tool effect already
started. Managed HTTP body upload also cannot extend the initial header
authentication, including when requesting a completed response replay.

Within the harness, independent reads may overlap; writes, arbitrary shell and
verification require appropriate exclusive workspace enforcement. Unknown tool
effects default to exclusive. The core supplies ordering/dependency identities;
the harness preserves read/write barriers and reports admission/execution
truth. Verification consumes the same permissions, tool limits and durable
result path as other tools.

## 10. Budgets, bounded operation and errors

v1 initial host defaults, configurable downward by a caller and bounded by host
policy: 4 active model attempts per run including root, 32 agents per session,
maximum child depth 4, 128 model attempts per run including policy/summary
calls and fallbacks, 8 outstanding workspace invocations per run, 32 queued root
runs per session, 128 pending messages per agent, 600 seconds active run time,
8 MiB serialized checkpoint, 16 MiB total unacknowledged output per session,
4 MiB ephemeral deltas per session,
and 64 KiB per user/control input. Effective limits are returned by capabilities
and frozen with each run. Host-wide admission limits additionally bound
aggregate sessions, model calls and memory.

Reserve resources before dispatch and reserve room for cancellation/terminal
records. Overflow produces a typed limit error and stops new work; it never
drops committed context, required results or operation identities. A harness
may enforce tighter limits. Tool-output bounds and artifact quotas are part of
its advertised manifest and checked at bind time. Active-time accounting
excludes intervals with no running model/tool work, such as pure approval or
commit waits; it does not sum overlapping work as wall time.

Artifact admission counts the current snapshot's distinct referenced bodies,
including hydrated archive dependencies and advertised material objects, plus
unconsumed allowances for newly admitted tool evidence. The initial per-message
body allowance is `artifact_quota_bytes / (5 * outstanding_tools + 2)`, frozen
with the invocation. Five first essential messages cover running, stopped and
unknown-effect observations and uncertain/definite outcomes. A definite outcome
releases unused allowances. An uncertain outcome does not consume the first
unknown-effect observation allowance: either message may arrive first, and
each retains its own payload and artifact-body reservation. Repeated optional
reports need additional room;
they cannot consume another invocation's reserved bytes. Current archive
representation and acknowledged/replacement-root overlap also count before ACK,
including before the first wire compaction. Aggregate failure uses the durable
capacity-failure path below, preserving the unaccepted request at the harness.

New invocations also retain `recovery_archive_allowance` in their checkpoint.
For each first archived running/stopped/unknown-effect observation, admission
reserves the frozen payload, its dependency metadata, archive entry growth and
a maximum-width activity handoff. The prospective archive and its replacement
must coexist within quota. Live status records do not consume these archive
allowances; definite results release unused ones. This reserves archive growth,
not a second artifact-body allowance for a phase already reported live. New
distinct bodies still require ordinary artifact admission. Repeated archived
observations and extra handoffs need fresh capacity. A missing allowance marks
a legacy invocation and does not retroactively narrow its reply contract.

These checks bound logical objects referenced by current state and its pending
replacement. They do not establish physical host storage leases, capacity for
all retained historical checkpoints, or all future recovery-archive growth.
Legacy invocations without a body bound have no prospective body reservation.
Those remaining storage and migration obligations require separate acceptance
evidence before the full cleanup guarantee is complete.

Checkpoint admission checks both the candidate and a conservative cleanup
projection. If a live run cannot admit the candidate, core discards it and
proposes an independent `run.capacity_reached` transition from acknowledged
state. Its checkpoint records `resource_constraint: checkpoint_capacity` and a
committed `resource_error`, pauses queued roots, requests cancellation of live
turns, and atomically fences unstarted tool invocations. The original request
has no acceptance receipt and returns `limit_exceeded / not_committed` after
the failure ACK; the changed head records the independent failure. Missing ACKs
remain `unknown` until exact head/batch reconciliation. A failed replacement
commit cannot reopen dispatch. Rejected tool evidence remains the harness's
responsibility until an operation receipt confirms acceptance.

Capacity failures do not imply elapsed-time exhaustion. Active-time failures
use `resource_constraint: active_time`; absent causes on legacy failures retain
that meaning. Both failures preserve uncertain effects and accept bounded
settlement evidence. Cleanup ends the run as failed, including after a later
explicit cancellation. Restore validates the failure event against its
checkpoint and preserves each run's failure facts across the supplied journal,
including intervening runs. Historical snapshots without cleanup capacity are
rejected before a replacement ownership checkpoint.

Record estimated, provider-reported, reconciled and unknown cost separately.
Hard monetary admission requires conservative reservations from supported
pricing/output bounds; unavailable bounds block that policy rather than imply
an exact cap. Unknown in-flight expenditure survives recovery. Child,
classifier, summary, failed-attempt and integration costs all count toward the
run. A task-quality success cannot be inferred from transport success.

Stable v1 error codes: `unsupported_capability`, `unsupported_version`,
`unauthorized_scope`, `stale_epoch`, `stale_revision`, `operation_conflict`,
`busy`, `limit_exceeded`, `checkpoint_conflict`, `checkpoint_unavailable`,
`artifact_unavailable`, `no_feasible_route`, `invalid_tool_result`, and
`recovery_required`. Each error states whether the operation was committed;
an ambiguous transport failure is resolved through operation/head lookup,
never by inventing a new ID and repeating a side effect.

## 11. Code placement and verified baseline

Baseline inspected: `d93ed73be2411992cc44e3234b6e7b11df1effb2`. No native
`bitrouter-orchestrator` crate exists in this checkout; other branches are not
implicitly covered by this audit.

| Location | Reuse / required change |
| --- | --- |
| [SDK routing](../crates/bitrouter-sdk/src/language_model/routing.rs), [pipeline](../crates/bitrouter-sdk/src/language_model/pipeline.rs) | Reuse model routing, capability checks, fallback and settlement; expose the shared plan/execution seam |
| [PolicyRuntime](../apps/bitrouter/src/policy_lock.rs) | Reuse named policy/model-effort selection; support explicit core signals without selecting twice |
| [Online workflow projection](../apps/bitrouter/src/workflow_state/online.rs) | Preserve inferred diagnostics; add a typed native fact path rather than treating inferred state as runtime truth |
| [Continuation](../apps/bitrouter/src/continuation.rs) | Preserve origin/credential/causal compatibility and actual adjustments |
| [PromptTransform](../crates/bitrouter-sdk/src/app.rs) | HTTP-only today; factor required common preparation so native calls get equivalent behavior |
| [SubAgentToolset](../crates/bitrouter-sdk/src/language_model/server_tools/sub_agent.rs), [NestedRunner](../crates/bitrouter-sdk/src/language_model/server_tools/nested.rs) | Existing self-contained nested completion is not a persistent agent scheduler; use an injected adapter for managed delegation |
| [Assembly](../apps/bitrouter/src/assemble.rs) | Current nested pipeline omits main policy hooks; managed children must receive the shared preparation/selection/settlement path |
| [Responses adapter](../crates/bitrouter-sdk/src/language_model/protocol/responses.rs) | Unknown item types currently get skipped; do not declare multi-agent compatibility from request-field passthrough |
| [ACP controller](../crates/bitrouter-sdk/src/acp/controller.rs) | Keep connection/protocol duties; it does not become the native scheduler |

Target placement: evolve/create `crates/bitrouter-orchestrator` for the scheduler,
agent/context state and harness-port contract; keep reusable model execution in
`bitrouter-sdk`; assemble routing policy, credentials and API adapters in
`apps/bitrouter`. The SDK must not depend on the orchestrator or app. Inject a
model execution/policy port where app-owned implementations are needed. Use
small private modules and concrete implementations, not a new generic plugin
framework or an additional `bitrouter-core` crate.

The production harness owns its storage implementation and workspace tools.
Core work supplies a deterministic fake harness and a protocol conformance
suite. Integrate an existing orchestrator branch by reconciling ownership;
do not run its old scheduler beside the new one.

## 12. Implementation stages

| Stage | Core deliverable | Exit evidence |
| --- | --- | --- |
| C0 Contract | Rust/JSON DTOs, negotiated capabilities, operation/error schemas and deterministic harness fixture | Round-trip fixtures, unsupported-feature rejection, identity/epoch/commit tests |
| C1 Execution | One root agent, shared model pipeline, client tool round trip and durable step barriers | Real model request with fixture tool execution; no dispatch before matching ACK |
| C2 Scheduling | Child creation/reuse, messaging, waits, fairness, bounds and cancellation | Observable overlapping independent model calls; stable attribution and cleanup |
| C3 Routing | Joint context/executor/model plans, fixed/auto modes and actual receipts | Continue, reuse and fresh-context cases; stale/infeasible plan rejection |
| C4 Recovery | Restore, ownership reconciliation, pending tools, queue and steering | Crash/ACK-loss/duplicate-delivery matrix passes without replaying confirmed effects |
| C5 API | Managed Responses adapter, harness channel and explicit compatibility surface | Independent client consumes events, returns results and reconnects; old inference behavior passes |
| C6 Integration | Production harness conformance, real-provider and pressure runs | Integrated acceptance below; limitations and actual measurements recorded |

Each stage keeps an executable path and adds its acceptance evidence to a
separate implementation record. No stage may mark later stages complete merely
because interfaces compile. Do not build production harness storage/tools as a
substitute for completing the core contract.

## 13. Acceptance matrix

| ID | Required scenario |
| --- | --- |
| A01 | In-process and remote harness clients drive the same scheduler and routing decisions |
| A02 | Ordinary model API requests do not create agents or require a harness binding |
| A03 | Managed opt-in, caller/session binding and unsupported-version checks precede execution |
| A04 | Root delegates two independent tasks with actual overlapping model execution; each result reaches its proper parent |
| A05 | Explicit spawn creates a new agent; delegate may reuse an eligible idle agent; follow-up preserves its context |
| A06 | Send does not start a turn; wait releases model slots; message replay does not duplicate consumption; wait cycles are rejected |
| A07 | Fixed overrides survive automatic policy; infeasible context/model/tool combinations never dispatch |
| A08 | Required instructions and call/result pairing survive context selection; material changes invalidate stale candidates |
| A09 | Native and HTTP model execution use equivalent constraints and accounting; selected and actual fallback configurations are distinguishable |
| A10 | Collaboration and workspace calls have one designated execution owner; duplicate delivery never launches another known invocation |
| A11 | Proposed/partial calls and wrong ACKs never start work; commit failure stops dependent scheduling |
| A12 | Lost ACK followed by reconnect adopts the committed head; conflicting retries fail; duplicate results are idempotent |
| A13 | Crash before/after model intent, complete output, tool dispatch, tool effect, result commit and terminal commit is exercised |
| A14 | Confirmed tool effects are not rerun; unknown shell/write effects block recovery; old epochs cannot start tools or commit new state |
| A15 | Old in-flight provider attempts are reconciled on takeover; their uncertain spend is retained and partial output cannot become actions |
| A16 | Queue/steer/cancel races preserve input identity; no conflicting unstarted work proceeds; restored cancellation is respected |
| A17 | Harness disconnect blocks new dispatch; slow UI/delta consumers do not block cancellation or durable control |
| A18 | HTTP exchange completion can mean waiting for tools; root run completion waits for descendants, verification and committed terminal state |
| A19 | Concurrent tool results retain attribution and workspace barriers; approval denial executes no effect |
| A20 | Context, tree, mailbox, queue, artifact and budget limits are enforced without losing terminal/cleanup records |
| A21 | Costs cover children, retries and context preparation; missing usage is not reported as a known zero |
| A22 | Unsupported Responses multi-agent items/features are explicitly bounded; no encrypted-item or full-beta compatibility claim without evidence |
| A23 | A production harness and an independent client complete, disconnect, restore and continue a real task using the same protocol |

Source changes must satisfy repository-required all-feature tests (including
doctests), clippy and formatting checks. Protocol adapters need captured schema
fixtures and official source links near manual integrations. CLI/configuration
or harness-wiring changes update the shipped skill/manifests as required by
repository guidance. A documentation-only freeze does not constitute any of
the implementation evidence above.
