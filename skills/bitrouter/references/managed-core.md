# Managed core harness integration

The `bro serve` inference listener also exposes the negotiated managed-core
profile. This is separate from native task/ACP execution: existing harnesses do
not automatically become durable managed-core clients.

1. Use an active `brvk_` virtual key on both HTTP and WebSocket requests. Managed
   endpoints require authentication even when ordinary inference uses
   `server.skip_auth: true`. The session namespace includes the user and key ID.
2. Read `GET /v1/orchestrator/capabilities`. Respect negotiated limits and
   `unsupported_features`; do not infer complete OpenAI multi-agent support.
3. Connect `GET /v1/orchestrator/channel` with the same credential and
   `BitRouter-Beta: orchestrator_core=v1`. Send a v1 `session.bind` envelope with
   the advertised core instance, harness grant, tool manifest and durable head.
   Persist `checkpoint.proposed` atomically and return its exact ACK before
   submitting work. A new empty binding receives its first checkpoint this way.
4. Submit `POST /v1/responses` with the same beta header, `multi_agent.enabled`,
   and the `bitrouter` execution/session/epoch/operation extension. Initial input
   is a task string. `stream: true` selects SSE. Core owns agent/model/context
   scheduling; the harness executes only authorized `tool.execute` commands.
5. Before tool execution, verify both the invocation's authorizing checkpoint
   and its response's completed pending-call map. Streamed function arguments
   never authorize an effect. Response completion may still leave the run waiting.
6. Retain result operation IDs. A continuation names `previous_response_id` and
   submits `function_call_output` items with `call_id`, string `output`, and
   `bitrouter: {operation_id, status, evidence?, workspace_revision?}`. Use the
   same operation ID and exact outcome if also submitting `tool.result` on the
   channel. The whole result batch and successor acceptance share one ACK.
   Verification and material requirements belong on the initial request;
   continuations inherit them and must omit those request fields.
7. On channel loss, stop dispatch and reconcile the same grant/head through
   `session.bind`. Retain uncertain operations and query their dispositions;
   never invent a new input operation merely because the HTTP consumer left.
   Durable state remains harness-owned. The current remote transport rejects
   restoration with running tools until its clock-handoff bridge is available.
   For other uncertain tool states use `session.restore` with fresh observations
   and cumulative activity evidence. A retained local owner is fenced first.
   If binding/restoration is interrupted, reconnect with the same grant and
   actual durable head: core retains its initialization and exact pending batch.
8. Release only settled sessions. `session.release` commits the ownership fence;
   close the channel after receiving the receipt to free the registry slot.
   If the release ACK is lost, reconnect with the original grant and actual
   durable head, then use `operation.get` with the original release operation
   ID. Adopting the release does not renew ownership or append a reconnect
   record. Restore the released checkpoint under a strictly higher epoch to
   resume work; retained queued roots remain paused until `queue.resume`.

An initial bind whose ACK was lost follows the same exact-head reconciliation
rule: reconnect with the original grant and the head actually persisted by the
harness, including the empty head if no proposal was committed. Do not replace
the session or accepted operation identity. Managed HTTP admission remains
closed until binding completes. `session.head` is read-only and requires the
actual current durable head; it does not cancel an active provider request.

On core process replacement, retain the harness journal and ownership grants
independently of the lost process. Validate its exact durable head, obtain the
new core instance from capabilities and restore with a strictly higher epoch.
A delivered command that the harness can prove never started may be reported as
`not_started`; core can redeliver the same invocation/attempt under the new epoch.
A stopped write without a confirmed outcome is not proof that it never happened:
report `effect_unknown` and reconcile it through authenticated restoration.
A live result or continuation may be retained, but execution stays in
`recovery_required` until restoration confirms the effect. A confirmed result
retained by the harness can be supplied during restoration even when its core
checkpoint or ACK was lost. Keep the original uncertain evidence and operation
identities; response replay does not authorize repeating the workspace effect.

Artifact quota must cover current bodies, outstanding tool-evidence allowances
and archive growth, including coexistence of acknowledged and replacement
roots. New invocation checkpoints retain `recovery_archive_allowance` for first
archived running/stopped/unknown-effect evidence and the associated handoffs.
Core rejects new tools when these obligations do not fit, including before the
first archive is uploaded. A smaller negotiated control/reply bound can reduce
this reservation; lowering an existing invocation's reply contract cannot.
Absent allowances remain legacy state. Additional distinct artifact bodies,
repeated archive observations and extra handoffs require fresh capacity. These
logical checks do not reserve physical staging or historical-checkpoint storage.

An `effect_unknown` result and an `effect_unknown` status report have separate
first-message reservations. Either may arrive first; preserve and submit both
when they represent distinct evidence. Optional repeated reports cannot consume
the first unknown observation's payload or artifact-body allowance. Definite
outcomes still require authenticated recovery after uncertainty.

After `run.capacity_reached`, retain rejected input until its own operation
receipt confirms acceptance. Continue committing cleanup and submitting the
frozen replies for dispatched tools. Core settles pending collaboration calls,
retains runtime wait results and archives child conclusions even when the
parent mailbox is full. A lost child-delivery ACK uses the same grant/head
reconciliation; it must not launch another tool or duplicate a parent notice.
The run ends as failed after its owned effects settle, then ownership can be
released. A capacity-failure ACK alone does not establish completed cleanup.

After the cancellation checkpoint is acknowledged, `run.cancel` stops live
provider executor futures throughout the run; `agent.cancel` and
`interrupt_agent` affect only the target subtree. While a cancellation is
pending, it fences new dispatch without aborting accepted provider work.
A definitively rejected request releases its provisional barrier.
Persist cancellation `tool_start_fences` in the same transaction as the
checkpoint, serialized against actual local tool starts; do not wait for a
later `tool.cancel` message to revoke a pending approval. Root failure includes
the affected descendants, and restoration reasserts retained cancellation
fences. A fence neither undoes started work nor resolves an unknown outcome;
report actual results under their original identities.
Core still waits for SDK settlement and workspace tool cleanup before a
terminal cancellation. A complete model result already returned to the SDK
retains its usage evidence but cannot start new effects for an interrupted
turn. Stopping a provider connection does not prove zero usage: incomplete
attempts and any unresolved integration phase retain unknown cost exposure.

HTTP examples and the full wire contract are in the repository's
`docs/ORCHESTRATOR_CORE_SPEC.md`. Client functions come from the bound manifest;
request-level `tools` overrides are rejected. Unsupported input fields fail
explicitly, including stateless transcript replay and provider encrypted state.
Collaboration observation uses `bitrouter.*` events. The core model pipeline
retains the key's authentication/policy hooks; credentials are volatile and do
not belong in harness checkpoints.

The preview host admits four sessions and sixteen HTTP consumers. Query actual
capabilities rather than copying these defaults into a client. Active descendant
turns include waiting agents and exclude the root; their limit is separate from
model concurrency. Finished contexts can be reused subject to the same limit.
Descendant-assigned queued followups reserve their next slot so another spawn
cannot block the assigning agent's completion indefinitely. SSE currently emits
output deltas after the exchange has reached its durable boundary; it does not
provide live provider-token streaming.

HTTP consumer slots remain occupied while the server body or downstream byte
chunks retain the response, even after execution has finished. Retry a busy
consumer with the same operation identity. Key-bound policy is checked by the
shared model pipeline on each new exchange; changing a policy does not reopen
an already completed response.

Credential validity is rechecked after managed body upload, after durable
admission waits before model/preparation/counting work, and immediately before
queued WebSocket output is sent. Revocation, expiry or unavailable authority
fences the channel and blocks new dispatch; an earlier valid ACK is not a
credential lease. Retain accepted intents and results for head reconciliation.
Cached replay rechecks after registry contention, and the current binding epoch
is checked again before returning a job or starting work.
These checks do not retract network I/O or tools already dispatched. An
in-process host with revocable authority must implement
`HarnessPort::authorize_dispatch` and recheck after its own delivery waits;
the default is for a trusted in-process embedding. That callback runs under
the core admission lock and must not call session mutation methods.

HTTP JSON and SSE serialize incrementally from a shared immutable exchange.
The `ephemeral_bytes` window counts allocated output chunks across consumers of
the session and, when lowered, the same frozen run. A logical response or SSE
event may exceed that window: drain its chunks to receive the complete result.
The server does not truncate text or replace a result with a size error merely
because the complete response is larger than the window. Server-side consumers
that retain `Bytes` must release them after copying or processing each chunk.

Harness WebSocket commands, receipts and checkpoints use a separately bounded
control lane under `unacknowledged_bytes`; UI consumers cannot spend that lane's
capacity. Socket staging retains its byte owner through flush and both halves'
cleanup on failure. Lowering transport limits during restoration can return
busy until previously retained output fits. Reconnect/rebind do not reset live
byte ownership. Managed remote execution requires positive output capacity.

Managed requests acquire a consumer slot before reading their body and use the
host input bound (64 KiB by default), then the negotiated session bound. Both
managed and ordinary Responses body inspection have a twenty-second deadline.
Ordinary inspection has four separate host slots and retains its existing
16 MiB body bound; managed consumers cannot occupy these inspection slots.

Managed model steps bound each provider HTTP response by the smaller of the
frozen root run's and session's `checkpoint_bytes`. Descendants share that
bound. It counts cumulative decoded entity bytes, including SSE framing,
keepalives, deltas and terminal data; it is separate from the reusable HTTP/UI
output window. Content-Length is only an early rejection hint: actual chunks
are checked before buffering or SSE parsing, including unfinished events.
Input-count responses also retain their stricter 16 KiB bound.

An oversized successful response fails the attempt before its output can become
tool actions. A rejected error body is discarded while its HTTP status still
governs rejection, authentication refresh and Retry-After. The receipt retains
unknown spend when usage is unavailable; exact operation replay does not retry
the provider. This ingress bound does not establish full physical memory
accounting or bound allocations inside custom executors and preparation hooks.

New model attempts freeze `canonical_output_bytes` and
`canonical_output_version: 3` before provider dispatch. Version 3 allocates the
frozen root's `checkpoint_bytes / [16 * (active_models + 1)]`, rounded down.
Descendants use the same policy. Admission reserves canonical receipt/event
bytes plus future history, provisional and terminal answers, child mail and
active Responses output. Open runtime waits reserve both cleanup views of the
answer and its complete retained context sources. New waits compete for their
additional capacity before acceptance.

A committed receipt still reserves delivery until the step is applied or
interrupted. Existing cleanup projections then account for the retained answer,
mail and wait results. The cost inventory preserves the original contract after
turn/run replacement; retired attempts keep receipt/event capacity for late
evidence, without applying output to a replacement turn. Evidence must satisfy
the frozen result bound and rejection byte limit.

SDK admission counts JSON before and after private-output sealing, including
custom executor results and escaping. Restoration checks policy consistency
and headroom before takeover. A missing version with a retained byte limit
preserves the original `checkpoint_bytes / [2 * (active_models + 1)]` policy;
version 2 retains its original eight-share policy; missing both fields retains
the earlier unversioned contract. Unknown versions
or inconsistent attempt/inventory fields are rejected.

A rejected complete result has no deliverable content in its attempt report.
`output_rejection` retains the byte limit, canonical usage counters and their
origin, without raw provider metadata; configured cost and cache observations
remain separate evidence. SDK settlement still receives the original result
and raw usage once. Neither live execution nor recovery after an outcome ACK
loss retries that rejected result or authorizes its tool calls. An exact
response replay retains its original failure. Missing usage remains unknown.
Late evidence remains attributed to its original attempt; a newer run, model
step or applied steering input cannot be failed by an older rejected output.
New model-originated `wait_agent` intents also freeze
`wait_output_version: 1`. Before admission, core reserves normal result, paired
JSON-string history, durable/Responses event and inherited context-source
contributions, including queue cancellation views. Pending model outputs and
workspace tool sources retain that obligation through receipt and pairing.
Harnesses must preserve the marker unchanged. Missing markers retain legacy
cleanup-only admission; normal legacy wait completion still needs available
space. Unknown versions, markers on other actions and policy changes within a
restored journal are rejected before takeover.

Version 3 also freezes `attempt_report_bytes` in the attempt and cost inventory.
It covers the complete serialized `NativeAttemptReport`, including actual
identity, error and pricing metadata, and equals the canonical allowance plus
the SDK rejection envelope for the frozen request/route. Before dispatch core
reserves report, receipt/event, cost-inventory and failure-delivery contributions.
A recorded error retains its prospective terminal reason/conclusion capacity
until application. Retired attempts preserve their original report contract.

Oversized reports become terminal `report_rejection` summaries. Version 1 hashes
the serde JSON report after canonical admission and estimation, before metadata
projection; this is distinct from the durable projected-outcome hash. Actual
provider/model and pricing identities become explicit byte-count/SHA-256
commitments. These hashes do not retain readable diagnostics or raw reports and
must not be presented as the selected route's actual serving identity. Numeric
usage, cache observations and known zero/nonzero configured estimates survive.
Unknown estimates remain unknown. Non-finite pricing also rejects the report;
rate values survive as exact `f64::to_bits` integers instead of JSON nulls.

The original executor result and raw usage still reach SDK settlement once.
Rejected reports authorize neither tools nor fallback, including after outcome
ACK loss or cold restoration. The SDK checks that the rejection envelope fits
before dispatch. Its envelope uses the SDK's controlled cache labels/reasons;
custom executor and estimator allocations retain their separate memory limits.
Harnesses must preserve these fields and cost variants verbatim. Legacy report
contracts remain unchanged; this addition is not a new public API endpoint.

New tool intents, later model prompts, future archive
growth, physical copies and trusted extension allocations remain separate
admission obligations. The delivery checks do not
establish complete memory/storage or end-to-end conformance.


New model steps also retain `auxiliary_output_version: 1`. Core admits complete
preparation, input-count, context-validation and provider-integration reports
before their work starts. The allowance is 4096 bytes plus the serde-JSON request
identity. Pending context validation also reserves its candidate history.
Harnesses preserve the marker and full report fields in checkpoints and replay.
Omitted markers retain legacy admission; unknown versions and journal changes
are rejected before takeover.

An oversized count report retains `report_rejection` version 1, its admitted
limit, a pre-projection serde-JSON byte-count/SHA-256 commitment, and the original
numeric count if present. Its `Unavailable` outcome stops further counting and
generation. Numeric count evidence alone cannot establish context fit after its
request binding was rejected. A failure diagnostic above 1024 serialized bytes
retains `failure_diagnostic` and a controlled terminal reason; the commitment
does not preserve readable diagnostics. SDK preparation and provider-integration
reports use controlled error categories.

Known preparation failures (including hook Deny), validation denials and count
report rejections remain terminal after outcome ACK loss or cold restoration.
They do not authorize re-running a failed guard; accepted steering and
cancellation retain their existing priority. A provider-integration phase error
alone still follows its enclosing attempt's authentication/fallback policy.
These additions use the existing checkpoint and managed Responses interfaces.


Run count limits and byte limits apply together. The default 32-agent tree,
depth four, four active model slots and eight outstanding workspace invocations
are independent ceilings. Reaching one ceiling does not promise enough
checkpoint or artifact capacity to reach all the others simultaneously. Each
new dispatch must also retain its complete frozen reply contract and cleanup
records. Parent models that consume child conclusions need their own admission.

Advertise the actual harness artifact quota before starting a run. Increasing
that quota does not enlarge the core checkpoint or JSON reply limits. Conversely,
unused count slots do not override a byte-admission failure. A typed resource
failure stops new work while the harness still reports definitive results for
already authorized invocations; required outcomes and terminal records must
survive reconnect and acknowledgement loss.

Restoration authenticates the entire supplied journal and final durable head
before reading any recovery archive. It then validates hydrated snapshots one
at a time. Harnesses still provide the original checkpoint bytes and preserve
all referenced archive dependencies; this is an internal buffer-lifetime change,
with no new message or smaller legacy reply allowance. Ordinary checkpoint JSON
keeps omitted optional fields intact during historical receipt validation.
