# Managed core harness integration

The `bro serve` inference listener also exposes the negotiated managed-core
profile. The explicit `bro task managed` command uses the selected daemon's
native managed endpoint and CoreSession; `task run` and ACP retain their
respective protocols. Product CLI execution is described below under native
managed tasks; independent library embeddings remain available.

For typed context routing, negotiate `context_views_v1` and set the task's
`routing.context` to `auto`. See [decision-native.md](decision-native.md) for
configuration and evidence semantics. `limits.outstanding_tools` bounds pending
workspace invocations; `limits.total_tools` bounds their cumulative count across
all workers in a run, including work whose worker turn has retired.

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

For uncertain `input.enqueue`, `input.steer`, `run.cancel` or `queue.resume`,
retain the original command, including `expected_state_revision`. Reconcile the
actual persisted head, then query `operation.get` or retry that exact command.
Changing its expected revision under the same operation ID is a conflict, even
when the new revision equals the current head. An enqueue receipt reserves the
run/turn IDs and remains immutable after activation or cancellation. Cancelling
a queued run pauses the remaining queue; resume only after active execution and
effects have settled. A steering receipt records acceptance, not application to
model context; cancellation may settle it without applying its text.

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
After recovery was blocked by missing or stopped evidence, include the confirmed
result explicitly in the authenticated restore request even if its live receipt
is already durable. Repeating that identical definite outcome confirms it;
unknown results and other unresolved tools still block dependent execution.

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
If archive staging or checkpoint append fails, retain the session handle and
reconcile its exact grant/head. Reconnect can restage the same immutable bytes;
partial staging is not a recoverable object. Preserve the old archive and all
dependencies until the replacement checkpoint is durable and retention permits
reclamation. When the original session handle survives, reconcile its pending
batch before replacing it. Actual owner/process loss still uses revocation,
confirmed quiescence and a new grant to restore durable state.

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

The repository's [service API](../../../docs/specs/service-api/README.md#managed-core-api)
and `bitrouter_ai::core` types define the wire contract. The
[runtime guide](../../../docs/development/agent-runtime.md) explains ownership.
Client functions come from the bound manifest;
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

For subscription transports such as `openai-codex` that cannot accept a
request-level output cap, an omitted `max_output_tokens` follows the selected
model's declared positive output ceiling. Explicit smaller reservations and
unknown ceilings are rejected before dispatch. Models that can enforce a cap
use 4096 only when no model capacity is declared. The SDK still verifies the final wire request. The managed Codex profile
supports separate instructions and ordinary function tools; controls that its
subscription transport would discard are rejected. Subscription usage does not
establish a per-token monetary charge.


## Native managed tasks

With `core` and `harness`, the local Rust `NativeSession::open_authenticated`
facade binds an in-process host to checked tenancy credentials and one retained
execution revision. Require all three
[tenant dispatch capabilities](remote-administration.md) and an exact current
execution binding before opening the host. Its journal is scoped to the authenticated tenant and stable principal; each model
step obtains separate execution admission. Close or cancellation retains the
host until checkpoint, tool and workspace cleanup is confirmed. This facade
adds no CLI, HTTP or SDK session contract. Enabling tenant dispatch does
not convert the existing managed CLI or HTTP entrypoints into owned sessions;
their Legacy fences remain.

The authenticated facade also contains a bounded stopped-owner
reconciliation branch for an actual fresh exclusive Unix/file-SQLite owner.
It requires a persisted direct-HTTP, tool-free, Learning-Off profile and exact
original Choice/receipt evidence; OAuth/Count work and unproved capture/tool
cleanup remain excluded. It records missing evidence as Unknown and stops the
old run before allowing an explicit new Run. Never delete an active workspace
marker, reinterpret a completed child receipt as a Decisions answer, or treat
this branch as general crash recovery. Available dispatch capabilities do not
prove that an old owner or workspace is safe to recover. The managed CLI behavior
below is unchanged.

With both `core` and `harness`, an explicitly imported `choice_v1` child binding
on the retained Generation route enables the existing `context_views_v1` planner
for that authenticated local facade. The hidden Decisions child has its own
fail-fast admission and receipt; capacity must cover the parent plus child.
Intent and outcome ACKs remain in the same native journal. No host decision
configuration, CLI option or HTTP session binding supplies this authority.
Unbound routes retain complete context without calling a host decision model.
Skill-bound evidence remains complete even when an owned Choice selects other
context; its source and consent cannot be replaced by a decision answer.
The child binding does
not grant dispatch authority; parent and child still require current grants
and their own admission capacity.

The authenticated native Generation profile can count provider input tokens on
one retained Responses target with static credentials. The optional reduced
context is counted again before generation; counting facts remain separate from
model attempts and token usage. Its request proof includes the executor's actual
streaming choice. `NativeSession` itself uses the persistent managed port's
default non-streaming delivery; a controlled caller observing streams binds the
streaming generation request. Owned native OAuth and server-managed continuation
references with counting remain unsupported and fail before provider work.

`bro task managed PROMPT [--model PROVIDER/MODEL] --workspace PATH --session NAME`
submits to the selected local daemon's versioned managed endpoint. The daemon
owns CoreSession, native BRO tools and resource connections; the CLI is only a
client. The daemon's original configured database stores
exact checkpoints, ACKs, artifacts, start fences and outcomes atomically through
revision checks. Omit `--session` for a fresh UUID. Output is NDJSON with
`managed_session` and `terminal`, extended with request/daemon/native identities;
this entry does not stream tokens. The CLI opens no database and has no embedded
fallback. Stderr prints the original identity before submit without claiming acceptance.
Managed v1 Hello declares a maximum of four active native sessions; excess
requests are rejected before native resource/owner admission.

`--read-only` omits writes, shell and MCP tools; explicitly granted skill sources
may connect for metadata/verified reads (see the CLI wiring reference). Coding mode approves its
own headless requests. `--check COMMAND` requests shell verification and conflicts
with read-only mode. `--effort`, `--config`, `--workspace` and optional `--model`
match `task run`; an omitted model uses the running daemon's startup default.
Native verification currently uses tool outcomes: a normally exited shell is
`Succeeded` even with a nonzero `exit_status`, which remains in its recorded
output. Nonzero alone does not fail verification; rejected/unexecuted checks do.
`--max-output-tokens` is an optional positive override; omission follows the selected model.
For an uncapped subscription route, set it to the known model output ceiling;
smaller reservations fail before dispatch. Pin Codex explicitly with
`--model openai-codex:openai/gpt-6.1-sol --max-output-tokens 128000`.
The registry currently leaves runtime token limits unset, so declare the route
ceiling explicitly in the config (using verified model metadata):

```yaml
providers:
  openai-codex:
    active: true
    models:
      - id: openai/gpt-6.1-sol
        provider_model_id: gpt-6.1-sol
        api_protocol: responses
        token_limits:
          max_output_tokens: 128000
```

Keep registry inheritance enabled for provider defaults. Credentials use the
existing provider store, including `bro providers login openai-codex --import-existing`.

AGENTS.md is required startup context. Skills and MCP instructions are frozen
material references; native permissions and inventory are delivered as core
signals. Core collaboration tools schedule child agents without a second native
model loop. Workspace exclusion is shared with existing native tasks.

A successful exit durably releases the session. Reusing its name in another
process continues context with the same canonical workspace and tool mode.
`--request-id` identifies one input: an exact duplicate returns its retained
acceptance without running it again; changed input conflicts. A new input key
after confirmed release keeps session/harness IDs and advances the epoch.
`--server-instance` fences submission to the original daemon boot. On a lost
submit ACK, the CLI queries the same key/boot and never implicitly resubmits.

`bro task managed-status --session NAME [--request-id KEY]` is passive and does
not start a daemon, restore a session or connect resources. It reports the
retained receipt separately when a newer run has replaced the original terminal
projection. `bro task managed-cancel --session NAME --request-id KEY --server-instance BOOT`
requests cancellation of that exact live input; inspect status for confirmed
settlement/release. Both support `--config` and an explicit owner-local control
`--socket`. Ctrl-C requests cancellation and waits; a disconnected/killed CLI
leaves its daemon-owned driver running. Daemon shutdown requests cancellation
and joins the existing driver/tools.
Joined processes with unknown effects remain recovery-required and unreleased.

MCP skill consent is independent of headless command approval. `managed` emits
`managed_skill_consent` and waits; `managed-status` projects original requests
without fetching bodies. Answer with `bro task managed-answer --session NAME
--request-id KEY --server-instance BOOT --consent-id ID --decision allow|deny`.
The original input, agent Turn, owner epoch, entry digest and permission state
must match. An identical retry returns its original `answer_receipt`; cold
inspection cannot accept a new answer, reopen an owner or fetch MCP content.
After restart, explicitly select the current daemon boot to read back the old
receipt; that read does not renew the original consent or its owner epoch.
Approval ACK precedes root-file reads. MCP instructions are source-tagged user
data, with the complete held entry, and are not shared into unconsenting children.
Root bytes persist in the original WorkUnit and are reverified before later
model steps or authenticated Core restoration. A resource-only source may be
offline; current source permission/configuration remains mandatory. Cache bytes
never become filesystem skill candidates. Native `skill_read` reads only held
manifest files and verifies original bytes on each reuse, including authorized
journal restoration. Lost owners retain their recovery block. `bro skills explain` reads original
native Thread/Turn or managed-session/input facts directly from the selected
database, without App/resource setup, owner claims or recovery. Prepared SDK
context and available provider-attempt outcomes are distinct evidence.

Unreleased sessions or workspace markers fail closed after abrupt process loss.
Running-owner handoff and automatic effect reconciliation are not implemented;
never delete authority records to force continuation. Full remote conformance
and physical storage reservation remain separate acceptance.

Native control input is bounded at 2 MiB, checkpoints at 64 MiB, checkpoint wire
bytes at 96 MiB, tool output at 512 KiB, and artifacts at 32 MiB. Admission can
reject work sooner when cleanup reservations cannot fit those bounds.


Native managed hosts expose existing local directory candidates to Core after
Turn acceptance. Core selects only explicit includes and persists directory
manifests, bounded choices and verified instructions in the original WorkUnit
before model preparation. Skill selection calls no decision backend. Ordinary
context preparation also keeps complete evidence; only the explicitly bound
owned Choice profile above permits a paid context decision.
Library callers can set
`TaskInput.routing.skills` include/exclude selectors; generic
`TaskInput.required_materials` retains its evidence semantics. AGENTS.md stays
required. MCP roots may be selected and explicitly approved through the same
Core ledger. CLI `--skill`/`--exclude-skill` flags carry exact scoped selectors;
explicit MCP URIs resolve metadata independently of listing before content consent.
Before final selection, changed user steering or signal/manifest facts rebase
the unfinished selection on the latest context and remaining budget. The original
catalog and explicit constraints stay intact. Selected directory manifests and
budgets remain frozen for that Turn; edited or removed files reject the binding
instead of reading a retained disk version.
Native `skill_read` reads only the selected original file manifest and delivers
verified UTF-8 content as source-tagged user data. Frozen Turn and final
model/context budgets apply without truncation. Local CLI/model fixtures cover
independent child consent, changed-directory conflicts and original file evidence;
external adapter delivery and real service qualification remain separate.

Native skill state requires both `native_skills_v1` and `context_views_v1` in
the acknowledged harness manifest/capabilities. Recovery validates the original
required features before decoding them as runnable state; an older core must
reject rather than discard skill or consent bindings. This native host feature
does not imply support by the independent remote managed HTTP profile.

Supporting-file state additionally negotiates `native_skill_files_v1`. The
`skill_read {skill_id, reference}` model tool uses the current selected, approved
parent and its original file manifest. It returns a metadata receipt; verified
UTF-8 bytes remain in the original WorkUnit and enter later model requests as
source-tagged user data. No file body enters ordinary tool history or the next
Turn. Local reads revalidate retained bundle bytes; MCP cache reuse rechecks
source authority and original raw size/hash. Files never execute or activate a
nested skill. A legacy Turn without a frozen file budget gets no new tool.
Known oversized/unlisted files are rejected before reading; mixed tool batches
still receive the final context/model checks once all results have settled.

Unreadable or invalid skill roots retain their original binding and cause in
Core checkpoints. Optional roots can be excluded before first activation;
required or previously loaded roots fail the original work rather than being
substituted. Stale reads cannot commit bytes after permission/context changes.

### Model integration boundary

Managed Responses runs through the generation pipeline. The ordinary native
Decisions API remains a separate operation; it is not the managed agent channel.
Core prepares failed tool results as explicit JSON error envelopes for models
whose wire lacks a tool-error flag, while durable history retains typed outcomes.
Context rebuild keeps the admitted route and price snapshot and rechecks
credentials and required context before another provider call.


### Explicit owned learner registration

With local tenancy, `core` and `harness`, an operator may set
`tenancy.learning_registrations` to a local JSON file. The ordinary Service
initializes H and cryptography first, then authenticates this file before
listeners and the daemon's single evolution scheduler are exposed. Omission
creates no registrations and does not delete previously authorized durable
schedules. The daemon selects owned or Legacy scheduling according to tenancy;
a closed NativeSession is not needed to resume a durable owned schedule.

The file has `format: 1` and at most 32 `registrations`. Each entry has a
`credential_env` naming the environment variable holding a tenant API credential,
and a `registration` containing `installation`, `tenant`, `id`, `operation_id`,
`expected_authority_revision`, `expected_active_revision`,
`expected_schedule_revision` (null for a new registration), `route` (a qualified
ResourceRef), `evidence_root` (the actual frozen Eval root), and `enabled`.
Use references/revisions returned by existing resource import and configuration
operations. The file supplies no permissions, scopes, expiry, numerical inputs,
or provider credentials. It stores no API token.

Registration requires current whole-tenant ResourceRead and ResourceDraft, with
coverage of the retained configuration's complete dependencies. Each wake uses
the original credential ID/revision and immutable grant ceiling. Rotation,
revocation, expiry, an obsolete active revision, and unsupported source ownership
stop new work. A replacement credential requires an explicit registration CAS.
An `enabled: false` update disables an existing selection; it is still authorized
through the same ordinary operation path. Registration operation IDs retain the
ordinary expiry/replay rules; remove an already-applied startup file when it is
no longer needed.

For the first native history profile, the owned worker consumes real completed
native experiment evidence and creates an immutable Checkpoint, Policy, all
affected Route revisions, and a ConfigurationRevision. It logs the ordinary
operation and configuration IDs. Activation and rollback use the existing
configuration operation with current permission and active/authority revision
CAS. Publication does not activate. Operational evidence can justify a bounded
Retreat; it does not become conclusive Human or paid assessment evidence.


### Explicit Human review of a native experiment run

With `core`, `harness` and an authenticated retained Explore experiment profile,
the local `NativeSession` facade exposes `review_run()`,
`submit_assessment(RubricSubmission)` and `retract_assessment(RevisionInput)`.
The existing `owned_native_experiment` embedding example shows a Human callback
receiving the checked review and returning the existing structured rubric. The
server normalizes author identity from the authenticated stable principal.
Review requires current whole-tenant ResourceRead; submission and retraction also
require EvaluationSubmit, original scope ceilings and an explicit current-head
CAS. Caller fields never supply a grant. There is no new HTTP operation.

The review uses `recorded-native-quality-evidence-v1`, with citations to actual
recorded task, final answer and tool facts. This version is distinct from ACP
evidence. It requires a complete acknowledged journal, artifacts, native
experiment Unit/Step edges, execution receipts and delivered trajectory/Eval
publication. An explicit selected Human judgement can contribute quality to
cohort acquisition and retained learned checkpoints; original operational Eval
rows remain Inconclusive and their frozen root is unchanged. Retraction removes
the selected judgement without reviving a prior revision.

This source profile is current-run-only: it refuses an archived/recovered run or
a run whose original model epoch is no longer the inspectable journal epoch. A
later run cannot retarget earlier citations or assessment rows. Keep the source
run inspectable through the existing owned learner acquisition. Closing its
original owner normally is supported; reopening/changing its run is not a way to
reuse evidence. Manual work shares 4096 rows, 32 MiB and its original 5 s deadline;
learner acquisition shares 4096 rows and 8 MiB. Human review never starts a paid
judge or activates a policy. Paid assessment is a separate explicit
`assess_run(checkpoint, expected_revision)` call requiring a retained assessment
profile and current grants. New logical jobs consume the immutable revision's
cumulative job allowance; capability discovery does not start an assessment.
