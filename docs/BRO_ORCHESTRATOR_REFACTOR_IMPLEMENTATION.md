# BRO Orchestrator refactor implementation

Updated: 2026-10-16. Contract: [refactor spec v0.2](BRO_ORCHESTRATOR_REFACTOR_SPEC.md).

Implementation is authorized by the user's subsequent instruction to proceed.
The spec's original document-only notice describes its review turn; it does not
describe the current task. This ledger records implemented changes and missing
proof, without promoting the spec's proposed implementation choices into new
product requirements.

## Scope and baseline

Started from `fe9df6f8157945d3559789926326a7effb468db1` with existing, uncommitted
test/check/retry cleanup. Those edits are retained. The refactor has not been
submitted or verified in hosted CI. Existing GoalStore, ExecutionStore,
ContextStore and native CoreSession/checkpoint contracts remain in place.
Future core persistence, full Turn-fixed context, dynamic graph editing,
hierarchical sub-Goals and owner handoff are outside this delivery.

## Decisions and source map

| Topic | Concrete direction and boundary |
| --- | --- |
| D1: Goal owner | One accepted owner Thread referenced by `Workflow.owner_thread_id`; the Thread records `owned_goal_id`. The existing Goal coordinator lease remains a host fencing fact, distinct from the logical agent identity. A separate `input_thread_id` points to a metadata-only input Thread whose `input_for_thread_id` records the owner. Trusted native worker Threads carry immutable `goal_work` sources; app Goal attempts link actual Thread/Turn identities. Managed ACP worker associations now use the same execution authority; complete recovery remains pending. |
| D1: admission recovery | Owner creation uses a deterministic, namespaced key derived from the already saved Goal ID. `ThreadService.find_thread_admission` reads the original caller-scoped receipt without a new request or model call. After acceptance, the Goal association can be retried with the same action identity. This key is not reused for worker or Turn admissions. |
| D1: persistence | `ThreadCreated` and checkpoints retain the optional associations. Goal model actions link their original purpose/source/requirements revision to actual Thread/Turn IDs in operation history. Action keys derive from those persisted identities. Legacy missing fields mean no recorded association; neither a read nor a projection invents one. No new database/table or cross-store transaction is introduced. |
| D2: generation and input | Planning, input scope selection, input interpretation, branch review and completion evaluation now use tool-free Turns through the existing admission, runner, commit, owner and shutdown authority. The input Thread can overlap owner planning without a second active Turn in either Thread. Both consume shared active-Turn capacity. Bounded typed decisions retain their existing operation/accounting path. |
| D3: resources | Coordination Threads record the configured route selector. Exact supplied prompts, credential-free prepared plans, admitted route indices and actual SDK attempt reports are retained in ExecutionStore; Goal model-control callbacks retain existing reservations/accounting and live source checks. A completed response requires acknowledgements from both stores. A selector is not actual provider/model evidence. Existing Core per-step context preparation and SDK fallback remain; complete native/ACP worker report association is pending. |

The daemon passes its existing ThreadService to GoalRuntime. It does not create
a second ExecutionStore owner. App generation submits managed model work;
only its common runner invokes the SDK. New native and managed ACP dispatches
use common accepted Turns. Unbound historical ACP runs retain their original
read/control reconciliation path until cutover is proven. An owner link does not imply a
dispatched Turn, adoption, verification, recovered execution, or file permission.

Coordination Threads are metadata-only: no native tools, MCP, skills, startup
project instructions or project workspace lock. They remain usable when the
original project directory is gone. Ordinary native Turn RPCs cannot execute on
them. Model dispatch still passes the durable writer fence, including a fresh
check after awaited Goal callbacks; this exemption never permits tool intents.
Generation completion means a recorded text response, not valid JSON, accepted
requirements, branch verification or Goal completion. The app checks the
admitted source before parsing and using the result. Cancellation awaits SDK
settlement and tracked worker cleanup; unknown provider cost is not converted
into known zero cost by the legacy display-only estimate field.

### Current execution/resource capability map

This describes current code, including paths still awaiting migration; it does
not claim the target shared-runner architecture is complete.

| Path | Current acceptance/execution authority | Resource control and evidence boundary |
| --- | --- | --- |
| Goal planning/input/review/completion generation | `ThreadService.start_goal_generation`, common admission/runner/commit | Owner or input Thread route selector; exact prompt, SDK plans and attempt reports in ExecutionStore, Root source/budget accounting. Tool-free. Current SDK fallback remains; no full Turn-fixed context guarantee. |
| Native Goal work | `ThreadService.start_goal_worker_turn`, native Agent/Core driver | Immutable prepared resources, accepted Thread configuration, prepared-input hashes, SDK router bindings, actual/fallback reports and tool observations are read from existing stores. Actual content receipt intervals use a shared process monotonic clock; local native overlap/isolation and checked dependency join pass. Omissions, physical-provider timing and complete history remain unknown; selections are not proof of use. |
| Existing standalone native work | Native Thread/Turn entrypoints on that same ThreadService | Existing grants, native context/routing, permissions, verification and recovery remain authoritative. Goal-associated Threads reject the ordinary entrypoint. |
| Managed external ACP Goal work | Common ThreadService admission/runner/commit; supervisor adapts process/protocol transport | Explicit ACP configuration and actual run/session mapping, bounded sequenced public observations, original permission options and stop receipts. Generic supervisor controls reject managed ownership. Private context, physical provider/model and unsupported controls remain unknown. Cold session continuation and the full failure matrix remain unqualified. |
| Direct harness and transparent ACP serving | Existing direct/transparent entrypoints | Preserve the original harness interaction and protocol capabilities; do not fabricate Goal/Thread/Turn bindings or turn private harness behavior into native evidence. |

The owner/input Thread split preserves input-model availability while owner
planning is active; it does not bypass the common active-Turn limit. Bounded
typed decisions retain their existing work/accounting identities and are not
presented as ordinary generated worker responses. Shared generation/native capacity
waiting is now implemented below; worker quote waiting, managed ACP ownership
recovery and worker evidence association remain S2–S4 work.

## Native worker foundation

`create_goal_worker_thread` verifies the existing Goal owner and a deterministic
work-source key. `start_goal_worker_turn` uses common admission, workspace
ownership, Agent/Core execution, commit and shutdown. Generic native RPCs cannot
bypass this source. Active restore requires a fresh host control; terminal cold
reads remain non-executing. A crash after admission but before the initial Core
checkpoint remains an explicit recovery gate, not a reason to generate a new key.

`GoalWork` binds the owner, Goal, stable task/attempt and task/control revisions.
Root accounting checks this exact work scope, allowing an independent branch
edit without accepting the edited branch's stale work. A changed Goal-wide
objective invalidates every existing task source even when the interpretation
omits individual instruction rewrites; scoped edits retain independent work.
Accepted observations can settle after pause/version changes. A native worker's
temporary quote contention does not itself halt the whole Goal or poison its accounting
acknowledgement. Bounded shared generation admission is connected; worker quote
waiting and active original-Turn continuation still need integration.

Core keeps routing/context preparation and native checkpoints. Optional host
controls only narrow the existing ordered routes and authorize aggregate work.
Tool launch and configured verification recheck the source. A source/budget
refusal is a work-admission refusal, separate from loss of durable connection
authority; in-flight outcomes and cleanup remain recordable. Native model output
requires the host ACK before returned tools can run. Native context decisions
have their own frozen-price reservation and outcome hooks, including failed or
cancelled decisions that report usage. Cancellation awaits the decision executor's
settlement, and committed decision replay reconciles the original attempt rather
than calling the backend again. Root typed settlement reads its immutable intent
receipt, so released reservations cannot change the content of a repeated ACK.
If a source closes after the intent commits but before dispatch, the known
no-dispatch outcome still releases that original quote.

SDK policy preparation now uses the same typed decision admission/outcome hooks
as Core context decisions. Managed generation stores those original typed
intents/outcomes in ExecutionStore and requires both its own and Root accounting
acknowledgements before generating. Rejection and delayed cancellation retain
known usage; a no-dispatch error cannot erase reported positive usage. Restored
Root controls use the original accounting operation's requirements version and
quote, including after independent Goal edits.

App native worker integration is now in progress. Plans can omit an external
harness; existing ACP IDs keep their serialized spelling. Built-in work uses the
common ThreadService entrypoint, selected MCP configuration, Ask permissions and
Root aggregate model control. Goal records link the accepted Thread/Turn rather
than inventing ACP run/session IDs. Read-only lookup reconciles missing links
using the original per-action keys, even after pause. Native permission cards
retain the actual input, tool and arguments, offer only once-only allow/deny,
and submit through ThreadService's original approval transaction. The existing
local-file policy can grant a scoped one-time choice; shell/MCP remain manual.
An accepted choice is not proof that its tool ran. Original-key lookup works
without loading a worker, and cancellation-generated denial observations cannot
be mistaken for accepted user choices. Pause/resume invalidates old allow requests.

Stopped native local-work reconciliation now reads the original admission and
bounded, fixed-cutoff Thread history, pairs model calls, tool intents and known
results, and retains content-addressed history pages plus a stable workspace.
It rechecks the terminal state/cursor before recording replacement evidence.
Raw model arguments and canonical dispatch JSON are compared by content.
Unknown, unfinished, external and unscoped effects remain blocked; a failed
local operation is not proof of no partial changes or consent. Existing retry
admission remains the only business retry rule. A controlled native write/check
failure now completes a fresh repair with original-effect proof, distinct
Thread/Turn identities and both workspaces retained; it does not replay the old
Turn. Shell denial remains blocked from automatic replacement.
Active cold recovery, worker quote waiting, complete native effect recovery
and managed ACP migration remain incomplete; controlled HTTP/tool fixtures do
not qualify live-model planning/review quality or the complete coding product.

For a saved native dispatch with no accepted Turn, the coordinator now reads the
saved prompt and re-submits the original independent admission keys. A previously
accepted Thread is retained; a live source/budget check precedes a new admission.
Queries still only reconcile existing receipts while paused. This closes the
pre-acceptance path in implementation, not the accepted active-Turn/checkpoint
recovery gate. After the loader issue was cleared, the SQLite close/reopen test
exposed missing host workspace registration. Native dispatch and evidence reads
now validate the exact managed Goal/attempt directory and canonical containment
before registering it. Arbitrary paths and symlink escapes are rejected;
existing registrations retain revoked permission profiles. Registration does
not resume execution or grant a once-only tool choice. The actual three-window
reopen regression now passes; complete active checkpoint recovery remains open.

`ThreadService.wait_goal_generation` replaces the app's unbounded Conflict/
Overloaded polling. Native Goal dispatch now calls `wait_goal_worker_turn`;
both use the same bounded wait implementation in `service/waiting.rs`, the
same acceptance fingerprint as admission, and the original independent action.
It checks exact-body original-key receipts, requires live
source authorization before new acceptance, and waits only for observed Thread
occupancy or active-Turn capacity. Native work may also wait for another known
live Turn's workspace reservation. A foreign OS lock without a known local
owner is refused immediately; an unknown-effect or cold workspace is not
classified as temporary capacity. The recorded Thread duration bounds waiting;
input/context limits, recovery, content conflicts, shutdown and revoked scope
are not treated as temporary capacity. No Turn slot or model call is accepted
merely for waiting. Shared lifecycle notifications wake the wait, with bounded
source rechecks; it does not add a scheduler, new store or protocol field.
Persistent UI wait-phase projection and worker token-quote waiting are separate
remaining work.

The worker regression holds an actual provider call and proves that no Turn
input, provider call or budget attempt was admitted while waiting. Releasing
the real Turn permits exactly one worker invocation; replay after source
revocation returns that original receipt and a changed body conflicts. Separate
cases exercise global capacity, same-workspace serialization, withdrawn source,
withdrawn grant, deadline, shutdown, input bounds, unknown local ownership and
an actual OS workspace lock held by a different runtime. The unknown-state
classification case sets service state directly; it does not replace the real
SQLite unknown-effect fault regressions.

### Original stop evidence

Stop observations now allow one refinement: an unknown stop can become confirmed
for the same original operation, under the current coordinator lease. Another
operation, repeated uncertainty and downgrading a confirmed result are rejected.
The original Thread/Turn or ACP run remains the evidence source. A changed run
intent does not erase the pending stop or create a replacement action.

Control driving includes dispatched attempts with native bindings even when
their Goal observation is unknown and has no ACP run ID. Previously unknown
stops are queried again without repeating the ACP stop RPC or native cancel.
Only a native terminal Turn with known effects, or the original owned ACP process
termination receipt, can confirm the respective stop boundary. Native
`RecoveryRequired` records an unknown stop rather than remaining requested
indefinitely. Repeated unknown reads do not append new operations.

Stopping the owned execution still does not reconcile external effects, accept
a replacement, verify a result or complete the Goal. The new database/native/ACP
regressions are being validated separately from the earlier full run. The native
negative case refuses the actual SQLite terminal-fact transaction; recovery
must leave the stop unknown and the original Turn in place.

### Managed ACP migration in progress

Thread configuration now explicitly distinguishes native `AgentConfig` from
ACP harness/model-request/duration configuration. Existing native JSON remains
unchanged; mixed native/ACP headers are rejected. An ACP Turn creates no native
Agent, model placeholder or Core checkpoint. Both drivers use common admission,
active-Turn capacity, the OS workspace fence, commits, cancellation and shutdown.

The supervisor retains actual process/protocol mechanics and records its shared
Thread/Turn owner. Managed startup sends no prompt and acquires no independent
lease. Generic mutation, lease takeover/removal and private session/model queries
cannot become a second controller. Adapter dispatch rechecks the common durable
owner and live Goal source after preparation or a transport mutation wait.
Original managed start receipts survive a lost process cache; a changed body or
owner cannot reuse them to launch another process. Missing Goal links query
original independent Thread/Turn keys before entering the historical path.

Public ACP observations carry bounded original sequence numbers, actual session
and negotiated route/capability reports. Multiple original permission requests
retain their offered options; a common committed choice is distinct from its
outbound dispatch and actual tool effects. A protocol end-turn response remains
separate from owned process termination and Goal verification. Incomplete prompt
history, uncertain stop or a failed durable ACK stays recovery-required. ACP
private history/context and physical model/provider usage are not native facts.

Cold reads retain associations but cannot authorize continuation. Confirmed same
session recovery, late stop/effect reconciliation, replacement and complete fault
coverage remain S3 work. The shared default ACP duration is 600 seconds; private
harness spending cannot be qualified by that bound. Direct harness and transparent
ACP serving retain their original mode.

The exact `managed-acp-source2.json` passes 26 focused app/service cases, including
actual ACP parallel handoff/delivery, no generic takeover, supplied model/MCP/skill
material and native permission/effect regressions. Source3 adds owner/source/grant
and durable-start-cache regressions: both pass, but its wider 74-case run has
53 passes and 21 stack-overflow aborts. It is not a passing acceptance result.
Source3's surviving crash fixtures are archived separately (2,103 regular files;
15 sockets/symlinks/other entries explicitly omitted). Independent extraction
verified every saved checksum; case-to-directory mapping and lost process/memory
state remain unknown.

Diagnostic probes found that supervisor queries included the startup future.
Boxing the awaited startup and receipt-query boundaries reduces the measured
dispatch future from 32,880 to 8,912 bytes, Goal task from 38,384 to 16,888 and
worker from 40,720 to 18,304 on this toolchain. The temporary probe is archived,
not retained as a product test. No stack limit or test requirement changed.
The subsequent source4 run has 73 passes and one stop/projection failure, with
no stack-overflow aborts. A permission refresh was advancing an already stopped
attempt's pending projection. It now uses the existing `worker_requested` source
predicate, preserving the original stop observation and avoiding repeated writes.
The unchanged assertion passes in the source6 full run below. S3–S6 remain incomplete.

## Compatibility

- Goal local protocol is **6** because history can contain worker choices in
  task updates and per-attempt worker provenance as well as owner/input/model
  execution links. Older clients/outboxes are explicitly rejected
  and retained without replay through the existing version check.
- Existing Goal snapshots/history remain readable. Missing ownership fields
  default to `None`; saved IDs and receipts are not rewritten.
- Native Thread projection adds optional Goal/input associations. Ordinary Threads
  still have no Goal. Generation source/request identity uses optional fields on
  the existing context/response projection. Native transport stays unchanged, but runtime root format
  becomes **9** so older writers cannot ignore and erase ownership. Formats
  2–8 remain readable; appending upgrades the existing envelope atomically,
  without history rewriting. New generation records carry exact inputs and
  SDK evidence; native records/checkpoints retain their existing semantics.
- A manually restarted old app Goal writer is a separate compatibility gate:
  rejecting old clients and native root writers does not by itself prove that
  an old standalone Goal loop cannot erase optional associations. Migration
  **000030** now records a database compatibility fence without new tables or
  rewriting history. A SQLite close/reopen test proves that the pre-refactor
  migration list rejects it through the pinned SeaORM resolver, while the new
  list preserves old Goal/history bytes. Downgrade of this fence is rejected.
  This tests the old migration list, not a released-binary matrix. Already
  running old writers still require the explicit quiescent cutover in S6;
  a startup marker alone cannot stop a process that initialized earlier.
- No CLI command, flag, provider credential or listen port changes in this
  batch. Shipped skill and CLI protocol documentation track version 6.

Worker replacement is connected through existing middle-input task
updates. A requested built-in/ACP worker versions the task and downstream
requirements; each attempt keeps its original requested worker separately from
actual Thread/Turn/session evidence. Existing stop, local-effect, dependency and
budget gates still admit any replacement. Legacy live attempts are frozen from
their previously recorded Task selection when that selection changes. Legacy
archives retain their exact bytes; a narrow Task field preserves their original
declared worker source. Neither field invents a session, accepted Thread or effect
proof. Migration **000031** adds only an application writer fence so a prior
managed-runtime binary cannot erase this provenance.

The replacement's prepared input names its original stopped-effect transcript
and frozen workspace. The app verifies artifact hashes and applies the local
result bundle to the new private workspace before admitting a worker. Changed
base/dependency versions or conflicting files block dispatch. These artifacts
are evidence and local results, not private harness context or new permissions.
Preparation-only retries retain the original transfer. Domain admission rejects
missing or foreign transfer references without changing the accepted state.

Controlled ACP-to-ACP/native replacement tests passed in worker-switch source4,
source5 and the final source10 focused run. The ACP peer checks inherited LEFT content before session startup;
the native path actually reads it before producing its own result. Independent
right work keeps its original attempt, and root delivery still requires checks
and semantic review. The one domain-test failure in those runs was subsequently
isolated to an incorrect fixture control revision and a missing replacement
workspace registration; source8 passes that corrected case. The required local
checks and exact source applicability are recorded in the worker replacement
checkpoint below. Remaining S3 recovery/interruption gates are still pending.

## Stage status

| Stage | Status |
| --- | --- |
| S0 | Mapped: current fact authority, owner/input scheduling and native/ACP execution/resource boundaries are documented; no core persistence dependency or placeholder executor. This mapping does not prove the remaining stages. |
| S1 | In progress: owner/input association, model and worker Turn source links, stable binding operations and original-key lookup implemented. Full interruption/compatibility proof remains. |
| S2 | In progress: managed generation and native workers use the common runner. Native permissions, scoped local effect reconciliation, pre-acceptance recovery and shared bounded generation/native capacity waiting are connected. Active cold recovery, worker quote waiting and full wait/effect projection remain pending. |
| S3 | In progress: explicit ACP configuration, common acceptance/control/permissions, actual session mapping, public observation and owned stop are connected. ACP-to-ACP/native replacement transfers confirmed local results, retains old sources and preserves independent work. Confirmed same-session recovery, late reconciliation and the complete interruption matrix remain. |
| S4 | In progress: generation/native SDK reports, prepared-input/router facts and actual content receipt intervals link to original work/Thread/Turn sources. ACP public session/route limits remain explicit. Complete historical qualification and final evidence/UI closure remain. |
| S5 | In progress: native and managed ACP dependencies/review consume common Turn facts. The native three-node fixture proves actual overlap, isolated conflicting local writes and checked dependency consumption through verified completion. Full coordinated/recovery/live closure remains. |
| S6 | In progress: protocol fixtures now have an unpublished minimal Cargo package; repeated test-time app/runtime builds are removed. Old execution-loop removal and final regression/live gates remain. |

## Acceptance audit

None of A01–A22 is claimed complete by these partial batches. Historical tests
and green checks for the pre-refactor MVP do not prove the new architecture.

| Requirement | Required remaining evidence |
| --- | --- |
| A01 | Refactored native Goal completes without external harness/core persistence; durable history/cold reads/recovery preserved. |
| A02 | Per-action replay/content conflict and owner/version fencing across all migrated paths; owner-only tests are partial evidence. |
| A03 | Native/Goal/ACP use the same acceptance and control authority; remove old parallel execution authority. |
| A04 | Owner, delegation/work and child Thread references; root integration evidence independent of worker success. |
| A05 | Policy reuse and recorded requirement/policy provenance with immutable accepted bindings. |
| A06 | Actual appended-input/adoption facts and truthful native/ACP control limits. |
| A07 | Incompatible changes, conservative continuation and actual fallback reports without replaying confirmed effects. |
| A08 | Delegation and dependency projection with separate edge meanings. |
| A09 | Valid checks and current handoff evidence unlock dependencies, not worker final text. |
| A10 | Frozen dependency consumption and rejection of stale/late evidence. |
| A11 | Downstream invalidation with independent work retained. |
| A12 | Stable work/requirements/history through edits and replacement. |
| A13 | Local native three-node acceptance now proves same-clock observed overlap, isolated conflicting scratch writes and checked dependency join. Preserve that evidence through the final source and live qualification; do not substitute admission or wall-clock timestamps. |
| A14 | Confirmed same-session restore retains Thread; new session/harness gets new Thread. |
| A15 | Replacement waits for confirmed stop/effect boundary. |
| A16 | Distinct worker/Turn action keys and interruption reconciliation at every specified boundary; owner-link recovery is partial evidence. |
| A17 | Input/control/modification phase evidence with queries remaining effect-free. |
| A18 | Negotiated ACP limitations, missing history and unknown private model/context shown accurately. |
| A19 | Permissions/revocation/root budget/accounting and unknown-effect safeguards across the shared runner. |
| A20 | Verified reviewable completion; explicit human acceptance, limits, blockers and control states distinct. |
| A21 | Compatibility and real database reopen without manufactured active identities or replay. |
| A22 | Three-view UI and direct harness CLI regression; detach preserves intent. |

## Verification log

- Initial orchestrator build progressed past the former macOS loader problem.
  The first sandboxed test run stopped when a wiremock server could not bind a
  local port (`Operation not permitted`). This is not a test pass or an
  application regression verdict; local-service tests require the appropriate
  execution permissions.
- Initial association checks pass: concurrent owner creation/content conflict and
  non-executing receipt reads; ordinary/legacy Threads remaining unbound and
  native grant withdrawal enforced; checkpoint
  ownership mutation rejection; real SQLite link-write failure followed by
  database reopen and original-identity reconciliation. Sources:
  [service tests](../crates/bitrouter-orchestrator/src/service/tests/goals.rs),
  [checkpoint tests](../crates/bitrouter-orchestrator/src/service/recovery/tests.rs),
  [SQLite interruption test](../apps/bitrouter/src/goal_runtime/tests/threads.rs).
- The S1 all-feature workspace run completed: **4,681 passed, one failed,
  24 skipped**. The failure was ACP initialization in
  `session_api_fresh_session_preserves_direct_routing_and_turn_timeout`.
  Its unchanged-build targeted reproduction passed. The original failure's
  cause is not established, and this is not a green full-suite claim.
- S2 app all-feature `cargo check` passed before the final fault-test additions.
  The focused generation/association/ownership run now passes **15 tests**:
  actual owner/input request overlap, single-Thread/global capacity, idempotent
  replay/content conflict, cold reads, missing workspace, rejected native RPC
  bypass, missing outcome acknowledgement, owner change during an awaited
  callback, cancelled-provider settlement and stopped-owner reopen without
  replay, plus existing ownership regressions. Sources:
  [generation tests](../crates/bitrouter-orchestrator/src/service/tests/generation.rs),
  [ownership tests](../crates/bitrouter-orchestrator/src/service/tests/ownership.rs).
- The broader service/Goal runtime regression completed: **206 passed**,
  including deterministic HTTP faults, real SQLite linking, native queue,
  steering, ownership, shutdown and cold-checkpoint recovery. Its process
  scenarios use Cargo-built Rust fixtures, not live provider qualification.
- The subsequent tool-boundary/steering changes pass **13 focused tests**:
  native steering cannot bypass a Goal operation; unsolicited tool output is
  retained as attempt evidence but never executed or accepted as a completed
  answer; ordinary native steering regressions remain green. Prompt preparation
  is also prohibited from adding tools/streaming to managed generation.
- The new compatibility marker, existing migration checks, SQLite link test
  and offline HTTP faults pass **18 tests** on the updated app source. Source:
  [compatibility gate](../apps/bitrouter/src/db/migration/m20240101_000030_bro_goal_execution_format.rs).
- The preceding generation batch passed workspace/all-target/all-feature Clippy with `-D warnings`,
  `cargo fmt -- --check`, whitespace and local document-link checks pass.
  That check predates the native worker batch. Full workspace nextest and doctests remain final S6 work after the other paths
  are migrated. The compiler's dependency future-compatibility notice remains
  distinct from a project Clippy failure.
- The current native/control batch passes **123 tests** across four binaries:
  managed generation/worker/owner service tests, Core context-decision and host
  route-admission tests, real-SQLite model-control tests, and the complete app
  Goal domain suite. This includes cancellation with delayed typed usage,
  original-quote settlement after lost ACK/pause, no-dispatch reservation release,
  independent task edits versus changed Goal-wide requirements, preserved route
  order/fallback identity, and blocked tools/verification after source closure.
  The first focused run exposed changing duplicate-ACK contents and source
  refusal being confused with loss of durable authority; both are fixed and
  included in the current passing run. Bridge tests use logical credits; monetary
  quote/CAS tests use SQLite. Controlled HTTP fixtures are not live-provider
  qualification. Sources: [native worker tests](../crates/bitrouter-orchestrator/src/service/tests/worker.rs),
  [typed decision tests](../crates/bitrouter-orchestrator/src/service/tests/worker/decisions.rs),
  [host route tests](../crates/bitrouter-orchestrator/tests/core_execution/host_admission.rs),
  [Goal model accounting](../apps/bitrouter/src/goal_runtime/model_control/tests.rs),
  [Goal input versions](../apps/bitrouter/tests/goals/input.rs).
- Current-batch workspace/all-target/all-feature Clippy with `-D warnings`,
  `cargo fmt -- --check`, whitespace and local document-link checks pass.
  Three legacy accounting fixture literals now explicitly have no worker source;
  boxing the optional source and managed generation job fixes enum size warnings
  without changing their persisted JSON or suppressing checks.
- That native/control all-feature workspace run completed with **4,699 passed,
  one failed and 24 skipped**. The process recovery test
  `native_process_loss_at_commit_windows_never_retires_owner_or_replays_work`
  failed because its fixture did not reach the settled commit window. Its
  source/configuration-unchanged isolated reproduction passed, but dependencies
  rebuilt, so this is not a byte-identical binary reproduction or an established
  cause. The exact test now joins the existing serialized `core-tree-pressure`
  group; no deadline, assertion or product limit was relaxed. A fresh final full
  run remains required. Output: `native-full1.log`, `native-process-repro1.log`.
- The subsequent auxiliary-decision batch passes **129 tests** (one reported
  leaky) across SDK policy routing, generation/worker controls, Core decisions,
  SQLite accounting, Goal domain behavior and that process recovery test. Source:
  [SDK routing control](../crates/bitrouter-sdk/src/language_model/tests_unified_routing/typed_routing.rs),
  [managed typed decisions](../crates/bitrouter-orchestrator/src/service/tests/generation/routing.rs).
  `aux-tests1.log` retains the result. The earlier auxiliary Clippy pass predates
  the final original-accounting-version helper and current native app changes;
  a fresh strict check is required. These results are uncommitted worktree
  evidence, not CI, live harness/provider or release qualification.
- App native integration passes **92 tests**: 90 Goal domain cases, a
  cancellation/schema-retention regression and one native inspection Goal.
  The latter has no external harness configured or built; it executes actual
  HTTP model streaming, a native file-read tool, independently run file checks,
  managed semantic review and final Goal completion over SQLite. Provider
  planning/review judgments are fixtures, not real-model quality qualification.
  Existing ACP JSON identity is retained, native work uses real Thread/Turn
  links without ACP IDs, and association after pause keeps its original source.
  Output: `native-app-tests6.log`.
- Earlier native inspection failures exposed a real in-memory connection bug:
  cancellation of a SQLx pre-acquire health ping could discard the sole
  connection and erase both Goal and metering tables. Memory pools now disable
  that ping using SQLx's documented cancellation remedy while retaining one
  connection; file/server pools are unchanged. The new regression cancels
  checkout futures and reads the committed schema/data afterward. This is not
  a persistence backend migration. After the fix, an incorrect fixture response
  (JSON for a streaming request) was corrected to actual SSE; no product
  streaming or verification requirement was relaxed. Failed runs remain in
  `native-app-tests2.log` through `native-app-tests5.log` outside Git.
- The final native app/fixture source passes workspace/all-target/all-feature
  strict Clippy and `cargo fmt -- --check`. The broader app regression passes
  **170 tests**: Goal input/model accounting, native inspection, legacy ACP
  parallel handoff/delivery, permissions, requirement/resource edits, recovery,
  database behavior and read-only projection. Several process tests spent most
  of their time building Cargo fixtures; their original bounds remained intact.
  Output: `native-app-regression1.log`, `native-app-clippy3.log`,
  `native-app-fmt-check2.log`. Whole-workspace/rustdoc/live gates remain separate;
  these partial results do not qualify native write-permission automation or
  complete recovery.
- Native permissions/local-effect reconciliation pass **110 focused tests**, including actual native write
  and file verification, manual shell allow/deny, pause versus late allowance,
  original approval-key cold queries, cancellation not counting as consent and
  the complete Goal domain suite. The new real-tool repair case verifies that a
  failed output/check remains in the original workspace, only a new Thread/Turn
  creates the corrected output, and replacement evidence contains the exact
  original admission/history pages and workspace identity. Fault cases reject
  missing/unknown/repeated results, mismatched calls, changed arguments and late
  outcomes; native search defaults stay separate from unreported ACP scope.
  Output: `native-recovery-tests2.log`. Earlier output
  `native-recovery-tests1.log` records 107 passes and one fixture-priority error
  before product execution; the fixture is corrected without relaxing product
  behavior. The first permission Clippy check caught a needless borrow in a
  legacy test; it is fixed. Current workspace/all-target/all-feature Clippy with
  `-D warnings`, `cargo fmt -- --check`, changed-document local-link checks,
  plugin JSON and whitespace checks pass. Output: `native-recovery-clippy1.log`
  and `native-recovery-fmt-check1.log`. These checks predate the subsequent
  metadata-path hardening and fixture packaging below; fresh checks are running.
- Full regression `native-recovery-full1.log` was stopped after **745 passes,
  nine actual test timeouts and ten interrupted tests**; 3,952 cases did not run.
  Timed-out tests were waiting for test-time Cargo fixture builds. This is not a
  green full-suite result. The three ACP/MCP/PTY peer sources now live
  in the unpublished `helpers/bitrouter-test-fixtures` package, and all builder
  consumers use Cargo's actual binary artifact. Its standalone build succeeds
  and contains no application, SDK, AI, orchestrator or ACP client artifacts.
  It imports the exact same provider schema types directly; the workspace's
  schema dependency explicitly enables that extension. The two import paths
  change, while the protocol implementation and test assertions are retained.
  No timeout or protocol behavior is
  relaxed. Output: `isolated-fixture-build3.jsonl` and its build log. The
  abandoned workspace-wide example-build experiment also compiled
  large dependencies and was not retained as a workaround.
- Review of the shared local-file predicate found a control-metadata alias
  gap. Both submitted and resolved paths now exclude Git/BRO metadata from
  automatic edits, including case variants and in-workspace symlink aliases;
  ordinary local writes remain eligible. The new regression and final full
  suite ran on this updated source in `native-recovery-full2.log`.
- That full run completed with **4,701 passes, eleven failures, five timeouts
  and 24 skips**. Metadata-alias protection and the formerly build-blocked
  protocol/startup tests pass, but the full suite is not green. The native repair
  case reached its verified repaired handoff and integration check, then timed
  out waiting for completion evaluation; an unchanged-source/configuration
  serial reproduction passes in 22.296 seconds (`native-recovery-repro1.log`).
  Cargo did not report a new compile for that reproduction, but no independent
  pre-run executable hash was captured. It does not explain the full-run failure.
  Several PTY/MCP/interpreter/registry cases
  also failed or timed out. A sample of an unresponsive PTY fixture shows all
  samples at `_dyld_start`, before fixture code. Its on-disk signature is valid;
  direct and byte-identical copied-file initialize probes did not answer in
  three seconds. Those probes were intentionally terminated and are not proof
  that the original test processes stopped. System logs contain matching
  syspolicy evaluations, without establishing the service's internal cause.
  The user supplied a privileged, read-only service sample: the scan queue stays
  in Security's signature parsing throughout the sample; a subsequent read-only
  process check reports about 97% CPU. This supports investigating a stuck
  security evaluation without proving every failed test shares that cause.
  A service restart has been requested because no non-interactive administrator
  authorization is available; it has not yet been confirmed. Do not attribute
  every failure to the loader or replace these failures with earlier passes.
  `native-recovery-source2.json` binds all 906 source/build/spec files for this
  run; a subsequent check confirms no drift. Logs, probes and the source
  manifest remain outside Git.
- The five identified lingering fixture children and their exact temporary-test
  parent have been terminated and confirmed absent. This is test cleanup, not
  product stop-confirmation evidence. The unrelated daemon and historical
  evaluation processes were excluded. Strict workspace/all-target/all-feature
  Clippy and workspace format checks pass for the source used by full run 2
  (`native-recovery-clippy3.log`, `native-recovery-fmt-check3.log`). The later
  native pre-acceptance recovery changes require fresh checks.
- Native pre-acceptance regression validation is not green:
  `native-admission-tests1.log` records an interrupted narrower-feature build;
  tests 2 and 3 record corrected test-only compilation errors (the prompt is
  checked in durable admission history, and the async result has an explicit
  error type). `native-admission-tests4.log` successfully compiles current
  workspace/all-feature library tests. Its two `--list` processes then stall
  before main. The sampled child is entirely at `_dyld_start` and strict
  on-disk signature verification succeeds. Listing was intentionally stopped;
  no test assertion was run. Both listing children and their launcher are
  confirmed absent after cleanup. `native-admission-list-sample1.txt` and
  `native-admission-signature1.log` retain the evidence. Current strict
  workspace/all-target/all-feature Clippy passes (`native-admission-clippy1.log`);
  whitespace and the final workspace format recheck also pass
  (`native-admission-fmt-check2.log`). `native-admission-source1.json` binds all 906 source/build/spec
  files and the compiled test binary. Only `goal_runtime/native.rs` and its
  `tests/threads.rs` differ from the full-run-2 source manifest. These later
  changes are not covered by that earlier regression.
- Rustdoc, a green final full workspace regression, real ACP/provider
  qualification and hosted CI are not yet claimed complete.

### After the 2026-10-16 restart

The pre-reboot `/private/tmp/bro-refactor-s1.YTOOyj` directory was cleared by the
restart. Its earlier results above remain historical observations recorded in
the chat/tool trace; those raw local files and manifests can no longer be opened.
Do not treat them as current-source verification or claim an available archive.
New evidence is kept outside Git in the persistent local directory
`/Users/kelsen/.codex/archives/bro-refactor/2026-10-16/`.

- The requested service-only restart was refused by macOS SIP; PID 491 did not
  restart. The user then rebooted the Mac. The observed boot time is 00:31:57
  America/New_York, the new service PID is 498, and its observed CPU use returns
  to approximately zero. No SIP setting or executable signature was changed.
- The exact pre-reboot test binary, SHA-256
  `4dce22c1143c9595ead4e0fef86fa599d63663ef5efba4077e691833cd99f619`,
  successfully lists 1,816 tests in 2,553 ms. `after-reboot-list1.json` binds the
  unchanged executable and observed result. This establishes that the prior
  startup stall cleared for that program, not the cause of every earlier failure.
- Directly running its previously unexecuted reopen test fails with a withdrawn
  workspace-grant error (`native-admission-compiled-repro1.log`). The managed-
  directory registration fix above is a product change, not a timeout increase
  or a bypass of permission checks.
- `capacity-and-reopen-tests1.log` compiles the changed sources and runs 20
  affected cases: nineteen pass, including the SQLite reopen/escape cases, all
  six actual native tool/permission/repair cases, source fencing and revocation.
  The remaining capacity-release test exhausted its single-response model
  fixture on the second actual request. The fixture now supplies both responses;
  its completion assertion is retained. The affected set then passes all twenty
  cases (`capacity-and-reopen-tests2.log`).
  `capacity-source1.json` binds the 1,286 workspace files for the first run.
- Fresh workspace/all-feature regression passes **4,720 tests**, with 24 skips
  and no failures or timeouts in 772.663 seconds (`after-reboot-full1.log`).
  This includes the actual native permission/repair, ACP/MCP/PTY process and
  crash-recovery cases. The original time bounds and protocol assertions remain
  intact. `after-reboot-full1-source.json` records the 1,286 source/build/doc
  inputs; `after-reboot-source-audit1.json` confirms no executable/build-source
  changes during the run. Only nine documents/plugin manifests changed.
- Current workspace/all-target/all-feature Clippy with `-D warnings` and
  workspace format checks pass (`after-reboot-clippy1.log`,
  `after-reboot-fmt1.log`). Workspace/all-feature rustdoc tests pass **six tests**
  with one ignored (`after-reboot-doctests1.log`). The test, Clippy, format and
  rustdoc processes all exit successfully. Local whitespace checks, the three
  plugin JSON manifests and the 199-line shipped skill also pass inspection.
- These results qualify the currently implemented paths. They do not complete
  the managed ACP migration, native parallel/dependency/adoption acceptance,
  remaining recovery and budget-waiting work, real harness/provider gates or
  hosted CI. S3–S6 and the final A01–A22 audit remain open.

### Subsequent stop-evidence verification

The later stop-evidence changes are not covered by the 4,720-test result above.
Their inputs are recorded separately in `late-stop-source3.json`.

- The first affected run passes 95 of 96 cases, including the new SQLite
  close/reopen and native stop-refinement cases, but the extended ACP test
  aborts with stack overflow. One existing archive case is reported leaky.
  `late-stop-tests1.log` is retained as a failed run. Strict Clippy and format
  checks pass for that source; they do not replace the failed test result.
- Boxing the added supervisor/control test futures avoids retaining them in
  the large parent test future. No stack-limit or product timeout is increased.
  The next run passes four execution cases and rejects a reused Goal-create
  key in the third ACP scenario. The fixture now uses its distinct case ID;
  runtime idempotency validation is unchanged (`late-stop-tests2.log`).
- All five current execution cases pass (`late-stop-tests3.log`), including
  actual ACP process exit after an unknown stop, native confirmation from the
  original Turn after resume, and refusal to confirm after the real SQLite
  terminal-fact transaction fails. The negative case retains recovery-required
  status, the unknown stop and the original receipt without repeated writes.
  The domain close/reopen case also verifies wrong-action/stale-owner rejection,
  exact receipt replay, no confirmation downgrade and no automatic effect
  reconciliation or replacement.
- Full run 1 completes **4,721 passes, one failure and 24 skips** in 841.279
  seconds. The failed external-editor PTY case retains its draft but does not
  observe the subsequent Enter in Goal history. An unchanged-source/configuration
  focused reproduction passes in 14.199 seconds; it does not explain or erase
  the failure. Strict Clippy, format and rustdoc pass for that source
  (`late-stop-gates1.json`, `late-stop-full1.log`,
  `late-stop-editor-repro1.log`).
- The editor fixture is then held until its own exact captured input proves
  startup, and the test waits for actual terminal input-mode restoration before
  accepting the completion frame. It retains the original ten-second completion
  bound, original draft/recovery checks, Enter persistence and owned-process
  cleanup assertions. All four Goals PTY cases pass; a separate active-Goal case
  is reported leaky (`late-stop-editor-tests1.log`). This does not establish
  clean process teardown for every case.
- Full run 2 still reproduces the editor failure and additionally fails the
  exact expected-error assertion in the unconfirmed-route startup case. The
  stronger fixture barrier alone therefore does not fix or explain the lost
  Enter. After those two actual failures, the remaining run is deliberately
  interrupted to implement the input correction: **4,716 passes, two actual
  failures, one interrupted test, three not run and 24 skips**. The later static
  gates are not run in that batch. `late-stop-source4.json`,
  `late-stop-full2.log` and `late-stop-gates2.json` retain this unpassed result.
- Source 5 removes Goal UI EventStream reconstruction. Its sole reader now
  performs a zero-time availability check and the guaranteed nonblocking read
  without an intervening await. Pending waits own only a timer; the editor
  disables this branch and has exclusive input. This avoids relying on
  EventStream's asynchronous reader shutdown during handoff. It does not alter
  Enter's Goal scope or submission semantics. Validation is in progress; the
  actual cause of every earlier failure is not claimed proven. Startup-rejection
  diagnostics now include the original error, without relaxing the expected
  route/MCP rejection or no-prompt/cleanup assertions. Editor failure diagnostics
  drain current output and retain the failing temporary fixture for inspection.
  Inputs: `late-stop-source5.json`. That attempted reader change is rejected:
  its focused run passes 20 cases but fails all four PTY cases on ordinary input
  (`late-stop-input-tests1.log`). The original Goal UI reader is restored byte
  for byte to source 4. The more informative rejection/editor diagnostics are
  retained; the editor-input issue remains unresolved. S3 and the remaining
  acceptance gates remain incomplete.
- Restored source 6 passes all 24 focused Goal client/resource/PTY cases
  (`late-stop-input-tests2.log`). The editor and route-startup cases both pass
  five fixed stress iterations (`late-stop-input-stress1.log`). These isolated
  passes do not resolve their full-run failures. A full run with retained
  failure fixtures and exact rejection errors completes **4,722 passes, 24
  skips, no failures/timeouts/leaky results** in 562.452 seconds
  (`late-stop-full3.log`). Current strict workspace/all-target/all-feature
  Clippy, workspace format and workspace/all-feature rustdoc checks all exit 0
  (`late-stop-gates3.json`); rustdoc passes six tests with one ignored. Only this
  document changed during those checks; executable/build-source hashes remain
  identical to source 6. This qualifies the current local checks, not the
  unproven input-reader hypothesis or the root cause of the two earlier
  intermittent failures. Both remain S6 diagnosis items. The rejected input
  experiment is absent from current code. The persistent archive's
  `late-stop-verification-summary.json` and final source audit bind these results.

### Shared generation/native admission wait

The app's native dispatch now uses the same bounded wait as tool-free
generation. The first focused build reports two compile errors (missing
`TurnRequest::Clone` and a test using `read` instead of `ExecutionStore::load`);
both are corrected. The next focused run passes nine cases; after adding
withdrawn-grant coverage and the real foreign-runtime OS lock case, ten cases
pass. The original compile output and both green runs are retained as
`shared-admission-focused1.log` through `shared-admission-focused3.log` in the
same independent local archive. This batch adds no protocol field, store,
scheduler or business retry policy. Token-quote waits and persistent UI waiting
phases are still separate unfinished work. The fresh full gate set is tracked
by `shared-admission-source1.json` and `shared-admission-gates1.json`. It fails:
**4,721 passed, four native Goal integration cases aborted with stack overflow,
24 skipped**. Clippy, format and rustdoc pass, but do not turn this into a green
gate set. The first progress scan missed `SIGABRT` markers; the final summary
and each case's stderr are authoritative. The shared async wait had composed
both large admission/recovery futures into its caller. Those driver futures and
the public wait boundary are now boxed, retaining default stacks, deadlines and
all assertions. The affected native Goal cases are included in the next focused
run; this correction requires fresh source-bound full verification.
That focused run is environment-blocked: its six app cases cannot bind the
loopback mock-server port inside the sandbox, before reaching worker admission;
the ten service cases pass. The next run uses the same source and test filter
with local-port permission. The blocked log remains
`shared-admission-focused4.log`, rather than being presented as a product
regression or a successful stack-overflow reproduction. The four surviving
crash fixture directories are preserved byte-for-byte (740 regular files) with
a manifest; four Unix sockets are explicitly omitted. These are filesystem
snapshots, not recovered in-memory SQLite/provider state, and their exact
mapping to the four test names is unconfirmed.
With loopback permission, the corrected source passes all sixteen focused
cases, including the four prior stack overflows, without changing stack sizes.
Nextest flags the existing revoked-decision settlement case as leaky in this
run (`shared-admission-focused5.log`); its exact cause remains an S6 cleanup
investigation, not a proven driver leak fix. Fresh full gates are tracked in
`shared-admission-source2.json` and `shared-admission-gates2.json`.
That full gate set completes: **4,725 passed, 24 skipped, no failed/aborted
cases**, in 582.802 seconds. Nextest flags one leaky exit in the pure
`typed_quotes_require_bounds_for_every_nonzero_price` case. This is a different
case from the focused run's cancellation settlement marker; neither marker's
cause is established. The full run is not described as leak-free. Strict
workspace/all-target/all-feature Clippy, workspace format and all-feature
rustdoc all exit 0; rustdoc passes six with one ignored. Only documentation
changes during these gates; executable/build hashes match source 2. The only
executable difference from failed source 1 is the boxed wait implementation;
its exact failed bytes were reconstructed and verified against the pre-run
SHA-256, with that reconstruction explicitly recorded. The crash-fixture tar
was extracted separately and all 740 file checksums verified. No new stack
size, timeout, assertion relaxation or test skip was introduced.

## Next execution work

Complete managed ACP session recovery and original stop/effect reconciliation
using the shared Thread/Turn authority. Preserve original
native/ACP action keys and transparent/direct harness behavior. A cold query is
not confirmed same-session continuation; an original startup receipt alone does
not authorize another prompt. Do not invent native context or repeat uncertain
effects to satisfy recovery.
Keep the earlier editor-input and route-startup intermittent failures visible;
the latest green run and isolated repeats do not establish their causes.
Close original-action recovery and late stop-evidence reconciliation, then
remaining token-quote waiting and its truthful UI projection; shared generation/
native capacity waiting alone does not complete resource waiting.
Native workers must retain existing CoreSession checkpoints while sharing the
root Goal's live control and aggregate budget; a full remaining allowance per
worker is not acceptable. Managed ACP session/process control must have one
owner, with confirmed stop before conflicting replacement. Replace the old
worker execution loop only when these consumers and guarantees have real
substitutes. Retain the newly proved local native overlap/isolation and dependency
consumption, and complete current-versus-historical adoption before closing S4/S5.
Run the required fresh source checks for subsequent implementation batches and
complete live harness/provider gates before the final A01–A22 audit.

## Managed ACP source6 checkpoint

`managed-acp-source6.json` binds the full repository file hashes to parent
`d2c5ad519c438d655b3ac43786c8e4a832d4f9b8` and the saved dirty-source patch/tar.
The source audit reports **zero changed files** throughout these commands:

| Required check | Actual result |
| --- | --- |
| Workspace/all-target/all-feature Clippy with `-D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| Full workspace/all-feature nextest | **4,730 passed**, 24 existing skips, 7 slow; no failures, aborts or leak reports; 563.544 seconds |
| Workspace/all-feature rustdoc | Passed; exact per-package results in the archived log |

The full run includes the formerly failing native stop/revision assertion,
original managed-start cache recovery and the shared owner/source/grant fence.
It does not establish the causes of earlier intermittent leaks or platform
failures, nor qualify pending cold-session/replacement/native-parallel gates.
No stack limit, timeout, assertion or skip was relaxed for this batch.

Raw failed trials, source snapshots, diagnostic probe, runners and crash fixtures
remain outside the PR under the independent local archive's
`MANAGED_ACP_INDEX.md`, `managed-acp-gates6.json` and source audit. This is a local
partial implementation checkpoint, without a push or hosted-CI claim. Final
publication must retain a separately retrievable archive and the necessary
Cargo-built Rust fixtures/reproduction instructions in the feature PR.

## Worker replacement source10 checkpoint

Worker selection now versions existing work through middle input. Existing
attempts retain their original requested worker; actual Thread/Turn/session facts
remain separate. New workers receive hash-verified stopped local results before
admission, and preparation retries retain the same transfer. Unsupported workers,
unknown old tool effects and stale permissions cannot bypass existing gates.
Legacy archive payloads retain their exact bytes. Goal protocol **6** and
application marker **000031** fence incompatible clients/writers.

| Check | Actual result |
| --- | --- |
| Strict workspace/all-target/all-feature Clippy | Passed |
| Workspace formatting check | Passed |
| Full workspace/all-feature nextest | **4,735 passed**, 24 existing skips, six slow; no failures, aborts or leak reports; 572.290 seconds |
| Workspace/all-feature rustdoc | Six passed, one existing ignored |
| Source10 focused Goal/input/UI/compatibility/worker suite | **116 passed**, zero changed files throughout the run |

The source9 before/after audit correctly reports one changed file and leaves
its aggregate `passed` flag false: the preceding formatting process finished
while the initial snapshot was captured. The only byte difference is line
wrapping/indentation of one test expression. An exact replacement audit proves
that no Rust token, assertion or production file changed between source9 and
source10; both original snapshots and that applicability record remain intact.
All four required commands passed, and source10's subsequent focused run has
an unchanged file manifest. This is not a claim of zero drift for source9.
Both source tars were independently extracted and all 41 saved files verified
against their original SHA-256 manifests.

Raw trials1–10, compile failures, failed assertions, source patches/tars and
commands remain outside the PR under `WORKER_SWITCH_INDEX.md`. Six accidentally
renamed foundation logs are explicitly mapped to their identical managed-ACP
originals; they are not worker-switch proof. Necessary Rust fixture source and
regressions remain in the feature branch. Normal trial temporary directories
and in-memory databases were not captured; the archive does not claim otherwise.

This closes a controlled replacement checkpoint, not the complete S3–S6 audit,
same-session cold continuation, native parallel/resource evidence, live harness
qualification or hosted CI. Subsequent implementation still needs appropriate
fresh checks; unchanged broad checks need no immediate repetition.

## Native execution evidence checkpoint

[`native_evidence.rs`](../apps/bitrouter/src/goal_runtime/native_evidence.rs)
now reads existing committed Thread history and Goal accounting receipts at
fixed cutoffs. It joins exact `GoalWork` sources with accepted Thread/Turn
identities, frozen startup resources, prepared-input hashes, SDK router binding
digests, admitted candidates and actual SDK outcomes. SDK preparation callbacks
remain distinct from model plans and provider attempts. Committed tool intents,
known results/errors and context-routing observations retain original IDs.
These queries neither start work nor write another execution state/store.

The view scans at most 1,024 recent revision positions per history, follows the
existing 16-item Goal page contract, and keeps at most 32 model work entries.
Original intent receipts resolve outcomes whose Begin precedes that tail.
Reports are hash-checked, each decoded artifact is capped at 2 MiB, model detail
at 64 KiB and the native view at 96 KiB. Truncated, oversized, changed or missing
material remains incomplete/unavailable; original receipts/artifacts are retained.
Queries for another execution cannot acquire the original worker's reports.

This exposes SDK serving reports, not physical provider attestation or proof
that every supplied skill/MCP tool was used. Core still prepares context per
step. Reports do not prove output adoption, permission, artifact verification or
Goal completion. Commit timestamps are not execution intervals, and
`timeline_complete` remains false. Verified native archive records now enter
the native reader rather than the ACP reader; a dedicated complete native
archive/recovery closure remains a later qualification gate.

Source1 captures the new completion-evidence requirement failing against the
old snapshot-only view. Source2 has 14/18 passes: native completion stays blocked
because the new reader requested 128 Goal history rows where the existing limit
is 16. Source3 retains the precise rejection; source4 passes after using the
published bound and distinguishing SDK preparation callbacks from plans.
Source5 passes native identity, tamper and actual HTTP fallback cases but has
one earlier worker-identity assertion failure (18/19). That test selected the
first historical attempt as the current worker. It now binds the latest actual
accepted Thread/Turn and asserts unchanged cumulative attempt count across live
and archived history. No stop/effect gate changed. The original transient state
was not captured beyond its assertion; the reason for its earlier attempt is
unconfirmed. Later passing repetitions do not establish that cause.

| Source7 check | Actual result |
| --- | --- |
| Strict workspace/all-target/all-feature Clippy | Passed |
| Workspace formatting check | Passed |
| Complete `goal_runtime` test selection | **74 passed** |
| Unknown-effect replacement guard, five stress iterations | **5/5 passed** |
| Workspace/all-feature rustdoc | Six passed, one existing ignored |
| Before/after source hashes | Zero changed files |

Source6 separately passes all 19 native/worker-edit/admission cases. Controlled
native completion requires actual SDK provider/model evidence. The HTTP fallback
case retains failed candidate/unknown usage and a distinct successful provider
under the same request and original Turn. Hash tampering removes the affected
report's serving claims; repeated evidence reads issue no provider requests.
These are local Rust/HTTP fixtures, not credentialed provider or hosted-CI proof.
The last full workspace nextest remains the preceding worker-replacement run;
fresh full workspace/final live gates are still required before publication.

Source8 retains a separate **73/74** run: the existing ACP requirements-edit
case failed during its initial worker startup, before the input under test.
Its transient database was not retained and the cause remains unconfirmed.
Later passes are not a fix or explanation for that failure. Sources9 and10
retain failed attempts to copy a memory SQLite fixture: no database file was
materialized, and the later read-only open failed. Their independently extracted
filesystem archives each verify all 251 surviving files; neither archive claims
to contain the lost memory database or original process state.

The native evidence case now explicitly uses a file-backed database and verifies
the existing SQLite snapshot helper by reopening the copy read-only and reading
the same Goal identity and nonempty execution history. Source11 passes that case
and the ACP edit case with file-backed fixtures. Source12 preserves the normal
storage profiles: only the explicit native snapshot case is file-backed. Strict
Clippy, formatting, all **74 Goal runtime tests**, both ACP edit/unknown-effect
cases for **five iterations each**, and six rustdoc tests (one existing ignored)
pass with zero changed source hashes. Nextest reports **one leaky test** in the
accounting lost-reply case; five isolated repetitions subsequently pass without
a leak report. The cause is unconfirmed; the earlier report is retained rather
than described as fixed.

Setting the test-only `BRO_GOAL_FIXTURE_KEEP_FAILURES=1` retains failed fixture
files, a bounded read of existing Goal/Thread/Turn facts, and diagnostic errors.
It does not change the fixture's storage profile or execute new work. File-backed
fixtures additionally reuse the existing preflight SQLite snapshot helper before
and after normal shutdown. Memory fixtures explicitly record that no full database
copy is available. These reads are separate observations, not an atomic snapshot
of all stores; private process state is not claimed captured. The flag is unset
by default. This adds no runtime storage or protocol surface.

Raw failed/passing logs, exact sources and commands remain in the independent
local archive under `NATIVE_EVIDENCE_INDEX.md`, without adding campaign snapshots
to the PR. This is a partial S4 checkpoint, not completion of S0–S6/A01–A22.

## Native activity and parallel-work checkpoint

The existing native harness adapter now forwards received content observations
to its host before lossy display delivery. A bounded buffer holds at most 32
request summaries per control and retains no content. Completed SDK attempt
reports carry optional versioned `bro_stream_activity` in their existing Goal
accounting artifact; no store, table, execution owner or SDK/Core schema is added.
Coordination generation keeps its existing streaming selection.

Each summary records a process clock ID, first/last monotonic receipt ticks,
content-fragment count and supplementary wall timestamps. Only strict overlap
of intervals from the same clock ID supports observed native overlap. Clock
IDs differ across processes; wall-clock regression is retained explicitly and
does not substitute for monotonic evidence. Missing/malformed summaries remain
unavailable without discarding valid serving reports. These are received-content
intervals, not full execution lifetimes or physical-provider compute timing;
`timeline_complete` remains false. Partial tool arguments grant no authority.

Repeated provider-outcome callbacks confirm the original report artifact and
reservation verdict instead of rebuilding them from an empty observation buffer.
The regression loses an outcome reply, settles the original receipt, pauses the
Goal and replays the same result without new work or changed accounting. A
different report under that key is rejected. Original activity bytes remain exact.

The new native three-node fixture sends real HTTP content to both independent
branches before either final tool-call chunk. Assertions compare actual stored
receipt intervals, not that server barrier. Both branches write different values
to the same ignored scratch path in distinct managed workspaces; each value
remains isolated. The integration worker actually reads both upstream files
before writing the join, and its frozen checked-dependency records match the
current exact handoffs. Host checks and separate completion review verify delivery.
Ignored scratch files are outside the Git-visible handoff, not claimed transferred.

| Source-bound check | Actual result |
| --- | --- |
| Source4 strict workspace/all-target/all-feature Clippy and formatting | Passed |
| Source4 complete Goal runtime selection | **76 passed**, no leak reports |
| Native overlap and lost-reply replay, five iterations each | **5/5 iterations passed**, no leak reports |
| Source5 shared worker regressions in the orchestrator crate | **9 passed** |
| Source6 native Agent regressions, including the long tool loop | **11 passed** |
| Workspace/all-feature rustdoc | Six passed, one existing ignored |
| Source4, source5 and source6 before/after manifests | Zero changed files |

Source1 retains the new observation assertion failing against the prior reader.
An initial runner-copy error happened before Cargo and is not a test failure.
Source2 passes the basic observation/replay cases; source3 adds monotonic evidence
and passes the complete native parallel fixture. Source4's app-package filter
does not select tests in another crate; sources5/6 explicitly validate their
owning crate. Source5's former native module spelling matches no such tests;
source6 uses the actual module name. Original command arguments remain preserved.
The source4 stress log retains its historical `unknown-stress` filename, but
its recorded arguments and contents are the native overlap/replay selection.

Raw logs, exact source patches/tars and runners remain outside the feature PR
under `NATIVE_ACTIVITY_INDEX.md` and its checksum manifest. Passing temporary
fixture directories were not retained; their Cargo-built reproduction source
remains in the PR. This is local Rust/HTTP qualification. A fresh final full
workspace run, live harness/provider checks, native archive qualification,
remaining recovery/resource work and the complete S0–S6 audit are still required.

## Delivery and evidence

The requested destination is the existing
[PR #981](https://github.com/bitrouter/bitrouter/pull/981), after the refactor is
complete. Its remote head was rechecked as `fe9df6f8157945d3559789926326a7effb468db1`;
the refactor batches remain local. Checkpoint commits preserve partial progress
and do not represent completion or publication; no refactor update has been pushed.
The PR has no conversation comments or inline review threads at that inspection.
Update its description and add the final implementation/validation comment
when publishing; report any intervening review separately.

Keep raw logs, failed attempts and intermediate snapshots in an independent,
traceable evidence archive. The existing pinned Git archive remains unchanged.
New refactor evidence currently lives in the local archive named above; it has
not yet been published to the independent Git archive. Bind the eventual
sanitized published archive to source identities and file checksums. The PR
retains the final acceptance summary and necessary reproduction material,
including the Cargo-built Rust protocol fixtures. A local archive or local
green run is not a pushed PR, hosted CI, live-harness qualification or completed
refactor.
