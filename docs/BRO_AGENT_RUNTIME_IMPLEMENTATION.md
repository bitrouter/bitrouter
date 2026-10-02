# BRO runtime v0.2 implementation evidence

Updated: 2026-10-01. Status: **the user-confirmed standalone runtime/crate gate passes local validation; full product MVP incomplete**.
Implemented scope: R1/R2, R3 context/queue/steering/history/observation, bounded
native/legacy reconstruction, fencing/discovery and explicit safe checkpoints.
The user confirmed standalone runtime/session completion and crate-only validation
before core integration on 2026-10-01. CLI/HTTP/ACP delivery follows separately.
The user confirmed that lost-owner/unconfirmed-effect records stay blocked in
this stage; host investigation/proof and operator resolution follow separately.
The core engineering spec is now available; its earlier availability blocker is
resolved. The historical [blocker audit](BRO_AGENT_RUNTIME_HANDOFF.md#implementation-blocker-audit)
does not block the current standalone runtime work.
Contract: [BRO runtime spec](BRO_AGENT_RUNTIME_SPEC.md), aligned with product 003
v0.2's execution requirements. **Authority refresh:** 003 now defers conflicting
ownership/interfaces/subsequent stages to orchestrator 004 v1.0. The existing
runtime evidence below is retained for migration and does not prove the new
core/harness implementation or C0–C6 acceptance. See the
[migration handoff](BRO_AGENT_RUNTIME_HANDOFF.md). The core spec was inspected at
`a93ea456f6eb69bcc17d9cf2fac63808ae81f20d`; integrating it is subsequent work.
Earlier [native implementation evidence](BRO_NATIVE_AGENT_IMPLEMENTATION.md)
belongs to the former runtime and does not prove the v0.2 acceptance gates.

## Phase ledger

R0–R6 below are the retained runtime v0.2 ledger. Do not relabel these results as
the new C0–C6 core stages or infer approval to build a second scheduling authority.

| Phase | Current state | Remaining gate |
| --- | --- | --- |
| R0 | Concrete storage/commit and compatibility choices being implemented | Complete Thread/control/recovery DTOs, limits, ownership and operator resolution |
| R1 | Transactional facts, commit acknowledgements, stable streaming Items and settlement implemented | Broader acceptance remains R6; recovery remains R4 |
| R2 | Bounded read workers, ordered barriers, global permits and verification integration pass local gates | Broader stress/provider/platform acceptance remains R6 |
| R3 | Context/FIFO/keys/steering and bounded history/observation/reconnect core locally verified | Broader races/stress remain R6, process reload R4 and transport delivery R5 |
| R4 | Bounded native/legacy reconstruction, fencing/discovery, terminal conversion and safe same-Turn checkpoint recovery implemented | Lost-owner termination/effect investigation and operator resolution are deferred host work; broader backend/platform crash evidence remains |
| R5 | Not implemented | Native/HTTP Thread controls and inbound native ACP |
| R6 | Not verified | Stress, real provider/client, platforms, full acceptance audit |

## R1 implementation choices

- `store.rs` defines the consumed execution-store commit/load interface, full
  model request/response facts, stable call Items, execution intents, results
  and settlement. A version comparison prevents concurrent overwrites.
- The app supplies a SeaORM implementation using its existing database resource.
  Migration 000022 creates `bro_executions` and `bro_execution_records`; version
  advancement and record batches commit in one transaction.
- Agent requests commits from the service owner and awaits acknowledgement before
  model work, effect execution or result consumption. Per-task commit guards
  serialize facts without holding the State mutex across database I/O.
- A failed commit cancels further work, marks recovery_required and retains
  workspace exclusion. The shipped host uses the database store; the explicit
  memory store remains for embedding and deterministic tests.
- Provider call identity is step-local; stable BRO Item IDs identify tools and
  approvals. Model-step and tool-call limits are separate. Cancel/bounds after
  full response admission settle all unstarted calls instead of returning an
  incomplete call/result history.
- Accepted user Items persist their identity before acknowledgement. Assistant
  Items allocate their identity before the SDK request; request/start/delta/full
  response/live snapshots share it. Cancelled, truncated or rejected streams
  commit bounded interrupted evidence, never a complete model-context message.
- Local protocol v13 reflects Item lifecycle, tool origin, optional Thread identity, queued Turn projections and steering status. No configured check reports
  not_requested. Continuous Thread/recovery commands are not published yet.

## R2 implementation choices

- One static ToolKind registration controls names, availability, validation,
  access metadata and dispatch. The existing read/ls/find/grep/write/edit and
  platform shell tools remain the authorized surface.
- Contiguous shared reads run through bounded owned futures, with up to four
  per task and sixteen global tool permits. File/directory reads run off the
  async executor; read rejects special files. Writes, edits and all shell calls
  drain prior shared groups before starting; later reads see their results.
- The state owner commits each result as it arrives. The model receives settled
  tool messages in original call order, including explicit unstarted results.
  Ordinary read errors stop further launches, collect in-flight work, then may
  be fed back in the next model step. Exclusive tools also return typed effect
  evidence: argument/path/edit-match rejections before mutation are NotExecuted
  and can be corrected in the next step. Temporary writes, directory creation,
  spawned-command failures, interrupted commands and lost workers remain Unknown
  until the operation returns a confirmed completed outcome.
- Cancellation, duration and store failures seal launches. Started futures are
  awaited rather than dropped, including their blocking workers and shell
  cleanup. Permits are released only after the worker returns.
- Verification uses the same registered platform shell, workspace execution
  lease, identified approval, global permit and intent/result commit boundary.
  Its Item origin is verification, with separate bounded evidence and cumulative
  time/call counters. It has no provider call and does not fabricate an orphan
  model ToolResult. Denial is explicit; interrupted effects require recovery.
- Approval waits hold no running-tool permit. Each current task reaches these
  waits only after its active reads settle, so only pure wait is excluded from
  active duration. Retained Thread settings/profiles and evidence projection into
  later Turns are implemented in the R3 core; transport delivery remains R5 work.

## R3 core implementation choices

- `thread.rs` defines consumed Thread/Turn DTOs. `TaskService` owns retained
  context, caller/settings/profile, FIFO and the existing Turn projections.
  Thread work uses the same Agent runner and commit acknowledgement path;
  Thread facts and wrapped Turn records share one versioned store stream.
- `create_thread`, `start_turn`, `enqueue_turn`, `cancel_queued_turn`,
  `resume_queue`, `cancel_turn` and `answer_thread_input` check the instance and
  trusted owner. Workspace profiles come from host grants. Default grants allow
  Ask/ReadOnly; AllowEffects requires the trusted embedding constructor.
- Admission and decisions persist their caller/Thread/operation-scoped key in
  the same transaction as the corresponding facts. Migration 000023 adds the
  unique database key index. Same request/key returns the original identity
  before checking busy; conflicting reuse appends nothing. Cached task eviction
  does not evict these keys. Cold retries return identity/status without workers;
  continuation requires the separate safe-checkpoint recovery operation below.
- FIFO admission does not change the active prompt. Activation obtains capacity
  and workspace exclusion, commits, then adds the new user Item to retained
  complete context. Start cannot bypass accepted queue entries. Provider call
  IDs can repeat in later Turns after their earlier call/result groups settle.
- Targeted withdrawal serializes against activation and does not release an
  active Turn's workspace. Failed/cancelled Turns pause the queue; resume is an
  explicit keyed decision that checks current grants/blockers. Temporary resource
  shortage is visible and retried when another Turn settles. Shutdown joins
  active work and preserves the unstarted FIFO with a durable pause checkpoint.
- Approval answers bind the current Thread/Turn/request and key. The decision
  commits before the one-use sender authorizes execution. Cancel intent commits
  before cancellation. AllowEffects applies consistently to model tools and
  verification; ReadOnly cannot configure verification.
- Complete settlement becomes the next Thread context. Verification evidence
  enters later context as explicitly labelled untrusted command output, without
  inventing a provider call/result pair.
- These are Rust core APIs. CLI/local/HTTP clients still submit one-shot tasks;
  their legacy idempotency is instance-local. No released Thread flag/route,
  automatic restart continuation or native inbound ACP is claimed. Steering is
  implemented in the Rust core; client commands/routes remain R5 work.
  Full hot-state/settlement/disk accounting remains an R0/R6 gate.

## R3 steering implementation choices

- `steer` targets the expected active Turn and current instance, validates owner,
  current grants and input capacity, then commits the received receipt, user text
  and scoped key together. Defaults allow 32 steering inputs and 64 KiB of total
  steering text per Turn. Conflicting/stale/over-capacity requests append nothing.
- Admission and tool dispatch share a short synchronous launch mutex. Admission
  seals launches before database I/O; a failed commit stays blocked. The launch
  boundary is owned worker dispatch, with its permit and a start handoff. A
  dispatched worker may wait for its start event; it remains owned and joined.
  Later calls get `not_executed_due_to_steer` without dispatch. Model, shared
  reads, exclusive tools and verification use this boundary.
- Steering does not cancel dispatched effects. Reads settle and modifying
  operations finish, retaining the workspace and permits until their workers
  return. Explicit cancellation, duration and storage failures keep their own
  stop/cleanup semantics. Existing SDK model requests may finish; their old
  response cannot dispatch stale tools after the seal.
- The existing owner handles the next model boundary under the Thread commit
  guard. Received inputs enter user context in admission order. Applied receipts,
  bound context versions/next step IDs and the exact ModelRequest commit together
  before SDK execution. Applied inputs are not injected again. A lost application
  commit never publishes applied state or starts that model request.
- Steering retires old pending approvals; new modifying work needs its own
  identified approval under the retained profile. The driver rechecks steering
  under the same guard before terminal commit and queue advance. Steering during
  verification resumes the same Turn through the same Agent with its messages,
  model/tool counters, spend and active-duration accounting, without replaying
  completed work or restarting its original user input.
- Failure, cancellation or bounds resolve remaining received inputs as not_applied
  with a durable cause. Storage failure cannot fabricate that resolution; the
  last committed received/applied facts remain authoritative for R4 recovery.
- This is core behavior. Bounded same-instance Thread history/observation and
  reconnect are implemented below. Process load/ownership/effect recovery and
  client-facing controls remain incomplete.

## R3 history and observation implementation choices

- `read_thread_view`, `observe_thread` and `thread_history` authorize the owner,
  current epoch and current workspace profile. The view includes retained
  settings, queue/pause, current or last activated Turn, steering, pending input,
  recovery blockers, terminal evidence and a bounded live tail.
- Every Thread commit adds one bounded public transaction event through the same
  commit owner, with the transaction's durable root cursor and timestamp. The
  projection excludes internal prompts, credentials and acceptance keys. SDK
  context still comes from complete execution facts, not presentation events.
- Full/interrupted assistant Items, stable step-qualified calls, tool results and
  verification evidence enter public history with the original authoritative
  fact. Losing a subsequent Task display-event commit cannot remove them. Later
  display updates refer to the same Items; completed Items ignore late deltas.
- Registration/subscription and snapshot cutoff share the state mutex used for
  publication. Disconnect leaves execution and approvals untouched. Attachments
  survive Turn end; a slow receiver atomically re-registers and receives a
  resynchronized snapshot. Delivery rechecks grants after awaiting new output.
- The first history page captures a fixed cutoff; later pages reuse it. Count
  and byte limits bound the store reads. The database fetches public rows within
  the requested range without loading internal prompts or whole execution
  history. Oversized events fail rather than being silently omitted. Historical
  events retain their originating epoch; the response envelope names the current
  serving epoch. Post-restart loading remains R4 work.
- Defaults: 1000 events maximum per history page, 2 MiB of event bodies per page,
  256 cached events / 2 MiB per Thread and eight attachments. Broadcast buffering
  applies a 32-entry / 8 MiB limit using the maximum event size, yielding four
  entries by default. The presentation snapshot has bounded metadata/live output.
  Global hot-state and durable retention accounting still require R0/R6 work.
- Live packets anchor volatile output to a durable cursor without advancing it;
  history does not replay deltas. Transient capacity waiting and storage failure
  send snapshots without fabricating a committed sequence. CLI/HTTP/ACP delivery
  and authenticated legacy Task projections remain R5 gates. Protocol v13 exposes
  the new runtime limits without publishing new Thread commands.

## R4 reconstruction implementation choices

- `ExecutionStore::read_records` reads authoritative facts at a fixed cutoff with
  both count and byte bounds. Memory and SeaORM implement it; the database reads
  records incrementally, detects missing sequence numbers and survives reopen.
  The loader never calls whole-stream `load`. Oversized first records fail
  visibly; later records are neither skipped nor silently truncated.
- `TaskService::load_thread` authorizes the first durable header against trusted
  caller, current grant and canonical workspace before scanning the remaining
  stream. Defaults allow two readers, 64 records/4 MiB per page and 1,000,000
  records per scan. Scanning holds neither the global admission nor State mutex
  across storage waits. Working state and installation use explicit bounds.
- Native Thread facts reconstruct admitted input, FIFO, latest activated Turn,
  complete context, controls, approvals and cumulative budgets. Model/tool Items
  and exact invocation associations validate; missing stable identities remain
  invalid rather than receive replacements. Interrupted model content is
  presentation evidence only. Invalid context cannot populate settled state.
- `RecoveryState` preserves the source epoch/cursor, recorded status/pause,
  pending steering and cancel intent, unknown calls and incomplete model steps.
  Unknown provider usage, call charging and active duration remain explicit;
  known counters are retained. Applied steering must bind its committed request.
- The reconstruction projection advances its writer epoch with committed facts
  before applying checkpoints. The live projection epoch filter must not discard
  a later writer's pause/status/queue state. Installed snapshots use the current
  serving epoch; durable historical events keep their original epochs.
- Installation always reports recovery_required and reserves local workspace
  exclusion. No model request, worker, approval sender or effect is created;
  completed calls are not replayed. Original approval metadata can be inspected
  without allowing stale answers. Cancelled and shutdown-paused FIFO stays paused.
  The recovered view participates in existing history/observation; public events
  retain their original epoch while the snapshot uses the current envelope.
- Accepted Thread, Turn and steering retries resolve receipts from authoritative
  fixed-cutoff record pages without retaining model contexts or checkpoints.
  These scans share recovery reader, page count/byte and per-Thread record bounds.
  Admission and Thread commit locks are released before retry scans, so a slow
  receipt reader cannot block unrelated admissions or controls.
- Loading remains an inspection gate. Explicit recovery, store fencing, shared
  workspace exclusion and startup discovery are separate operations below.
  Lost-owner investigation and operator resolution remain deferred; local
  exclusion and a new epoch do not satisfy them.

## R4 legacy Task read adapter implementation choices

- `load_thread` now accepts an original `Accepted` Task root. Original Task ID
  aliases both Thread and first Turn, with the same stable user/model/tool Items.
  Original records/cursors are never rewritten. `source_is_legacy_task` identifies
  the source in the consumed recovery report. Missing local/launch flags are not
  inferred from stored key/user IDs. Acceptance fingerprints remain original facts;
  loading does not promote the legacy instance-scoped deduplication table.
- Startup and explicit loading share `Rebuild`/`Active`; the separate legacy
  startup state machine was removed. Input/config/fingerprint consistency, event
  identity/order, calls/results, terminal facts, legal context and working-state
  bounds validate together. Legacy coding uses ask; read-only mode stays read-only.
  A legacy Task has its original writer epoch; later writers need an explicit
  conversion contract. Facts after the final outcome or unexpected native controls
  cannot be a valid legacy continuation checkpoint.
- `LegacyTaskProjection` is consumed by reconstruction and both history backends.
  Public cursors use root record positions, while embedded old Task event sequence
  and stable IDs remain unchanged. Full responses/results are projected; SDK prompt
  snapshots and settlement message arrays are not public page content. Unrecorded
  timestamps use zero; event timestamps/epoch remain original.
- Count/byte limits and fixed cutoffs apply. The database decodes one raw legacy
  record at a time, with a 4 MiB raw/header processing bound; sequence gaps and
  oversized records fail visibly. Native history keeps its public-row path.
- Every cold legacy view remains recovery_required, with cancelled tokens and no
  runnable approval sender, worker, SDK request or tool effect. Pending approval,
  unresolved intent and cumulative budget metadata remain inspectable. Loading
  adds no conversion, key/permission promotion, operator resolution or worker.
  Explicit terminal conversion below is a separate transaction; no new local/
  HTTP RPC is delivered.

## Standalone explicit recovery implementation choices

- `recover_thread` is a tracked owned operation. It rechecks authenticated caller,
  current epoch/grants, the inspected source cursor, legal bounded context, all
  effect/budget blockers, source stopped proof, complete startup scan and held
  workspace lock. A newer writer is required; stale, foreign, active-owner,
  unfenced, unknown-effect and uncertain-budget records cannot resume.
- The original recovery batch atomically records `ThreadRecovered`, accepted
  key, retained Thread checkpoint and public event. Installation/worker launch
  follows ACK only. Lost ACK adoption requires an exact bounded reread of that
  batch and head; unreadable/failed evidence withholds stopped proof. Client
  detachment leaves the owned operation alive, and concurrent identical retries
  append/launch once. Queue pause and original FIFO identities are preserved.
- Known terminal legacy settlement converts without rewriting the journal or
  Task/Thread/first-Turn IDs. Permissions remain as recorded; active legacy and
  unfenced owners are blocked. Converted history consumes later native public
  events while retaining original prefix cursors/epochs.
- The existing Agent emits `RunCheckpoint` before each model boundary with
  paired context and cumulative model/tool/spend/active-time counters. The
  reconstruction validates context/counters against durable execution facts.
  Explicit recovery can continue the original native Turn with those counters,
  original controls and fresh next-step identities through the existing runner.
  Completed calls are not rerun; consumed budgets are not reset.
- `Settled.outcome` retains final status/answer/detail. Recovery completes that
  original outcome without asking the model again. Known verification status/
  evidence are reused without running the check again. Old active settlements
  lacking an outcome stay blocked. Partial streams never become model context.
- Workspace inspection becomes an execution claim only after source stopped
  proof and checkpoint validation. Terminal inspection writes valid idle marker
  evidence; matching active claims may advance to the newer writer under the
  held kernel lock. This is cooperating-runtime fencing, not an OS sandbox.
- No client commands, new core scheduler, owner TTL or force-takeover shortcut
  are added. The user deferred unknown-owner/effect investigation to host
  integration; full product R4 remains wider than this independent crate gate.

## R4 store ownership implementation choices

- `ExecutionOwner` identifies instance/generation and an optional durable stopped
  proof. `claim_owner`, `read_owner`, `stop_owner` and `commit_owned` are consumed
  by the service, recovery reports and app backend. A new store can be claimed;
  an active old owner and legacy unfenced facts block new execution. There is no
  expiry/PID-based takeover. A stopped instance cannot claim again.
- Migration 000024 supplies a singleton write fence and retained owner proofs.
  Claims, stops and execution commits lock that fence in their transaction.
  Owned commits atomically validate the active owner, compare record version,
  and append keys/facts. Bootstrap/import writes cannot bypass an established
  owner. Independent connections do not each acquire execution authority.
- All service commit paths use the owner check: legacy one-shot admission and
  facts, Thread creation/controls and wrapped Turn facts. The native host
  initializes within its serving lifetime after fallible setup. Blocked native
  execution does not prevent inference serving. Local protocol v13 capabilities
  expose ownership state; no owner-resolution command/route is delivered yet.
- After admission is sealed, workers joined and queue pauses committed, shutdown
  can record the stopped proof. Lost commits and uncertain effect/cleanup facts
  conservatively prevent it. Stop failure preserves the active owner. A later
  instance can transfer only from a durable stopped owner, with old commits fenced.
  Recovery views retain the original owner's proof without releasing Thread
  recovery blockers or resolving operation effects.
- This is store-wide fencing, not the full R4 execution-ownership gate.
  Shared workspace exclusion below covers cooperating local runtimes using
  different stores. Lost-owner/process investigation and
  recorded operator resolution and effect investigation remain deferred. SQLite evidence does
  not prove other databases or arbitrary shell/OS isolation.

## R4 shared workspace implementation choices

- Local contract v13 uses the existing version handshake to reject a daemon
  that predates shared workspace exclusion before submission. Thread/recovery
  transport operations remain R5 work.
- Native one-shot admission, Thread start and FIFO activation obtain the same
  canonical workspace file lock independently of database selection. Parent
  sidecars use SHA-256 of the canonical UTF-8 path, stay outside model-visible
  workspaces, and require a writable parent. The regular lock file is retained;
  the version-1 marker has a 64 KiB bound, atomic replacement and file sync.
- The guard/marker bind lease, execution, epoch and owner generation. Kernel
  conflict is capacity waiting. An active, missing, invalid or oversized marker
  never becomes an execution grant just because a process is absent; coordination
  symlinks/nonregular files are rejected. ModelRequest/ToolIntent commit paths
  validate current local identity and the exact marker before SDK/effect dispatch.
- Normal terminal handling first commits WorkspaceReleasePrepared, then writes
  idle while holding the kernel lock, then commits the outcome and releases the
  guard/local reservation. Unknown effects and failed preparation cannot publish
  completion or release. A lost terminal acknowledgement retains the guard and
  blocked store owner even if the known-clean marker is already idle.
- Coordination I/O runs as tracked blocking jobs. Shutdown joins them; a dropped
  caller does not detach an acquisition or receive a false clean-stop proof.
  Cold reconstruction attempts an inspection guard, preserves existing claims
  and cannot finish them. A missing claim creates inspection-only active evidence,
  never execution authority or an automatically releasable idle marker.
- One tracked 100 ms FIFO waiter retries live external capacity; it exits when no
  queue is eligible or the service closes. Unconfirmed release commits a recovery
  checkpoint, not a resumable ordinary pause. No rejected start consumes its key,
  injects its prompt or dispatches a model. An uncertain admission acknowledgement
  conservatively retains an acquired guard without creating a worker.
- This is cooperating local-runtime exclusion, not a process sandbox, distributed
  filesystem guarantee, parent-directory fsync/power-loss promise or complete
  recovery. Known checkpoint continuation uses explicit recovery; lost-owner/
  effect investigation remains deferred. No marker-deletion or force-takeover
  command exists.

## R4 startup discovery implementation choices

- Migration 000025 backfills a monotonic root index in the database; root creation
  and index insertion share the facts/keys transaction. Owner acquisition reconciles
  roots from older cooperating writers under the singleton owner lock. The memory
  backend implements the same fixed-membership paging with current head versions.
- Execution initialization requires complete discovery after claiming the writer.
  Acquired-token caching cannot bypass failure. A failed scan is visible and can
  retry before admission. Without a claimed writer, bounded header enumeration
  provides metadata only and never classifies a workspace as clean.
- The scan uses bounded record pages and the same consumed recovery validator as
  explicit native/legacy loading, including stable call/result associations. Latest
  durable event/checkpoint epoch selects the owner proof, not permanently the
  original Thread header epoch. Only valid terminal settlement/checkpoint, known
  results and stopped source ownership permit fresh work on that workspace.
- Cold metadata installs separately from hot context, workers and approvals. Every
  execution entrypoint checks it, including legacy submission after initialization
  can add new blockers. An unresolved cold workspace blocks queue activation with
  a recovery checkpoint; unrelated granted workspaces can still run when the store
  owner was safely acquired. Existing queued/cancelled/paused decisions are not run.
- Defaults: 1024 roots, 1,000,000 scanned records and 4 MiB of metadata; page count
  and bytes use the existing recovery limits. Aggregate v13 capabilities expose
  progress/completion/fence/error state, without leaking root identities or prompts.
  Discovery alone does not convert legacy Threads, resolve operators or continue
  execution. Explicit recovery remains separate; full host crash recovery is deferred.

## Evidence so far

### CI compatibility follow-up (2026-10-01)

Validated source: `ce7a1291a7ff98b487cf4860e32485cf9e85d732`, from the clean
isolated PR checkout with Rust 1.97.0. The following ledger commit changes docs only.

- Independent orchestrator crate: **102 passed, 0 skipped**, run
  `fba91571-a6a3-4008-989b-9c6f4df02e14`.
- Full workspace: **3677 passed, 22 skipped**, run
  `5f958863-382a-4f6a-a503-4a285ed4028d`; no leak reported.
  The preceding attempt stopped after a compatible CLI worker exceeded its
  two-second probe deadline. The unchanged probe passed in the complete rerun;
  the transient failure has not been root-caused.
- Clippy all features/all targets with `-D warnings`, formatting and diff checks
  pass. Strict workspace documentation with `RUSTDOCFLAGS="-D warnings"` passes;
  workspace doc tests report **5 passed, 1 ignored**.
- The old PR head's Rust 1.99 CI exposed a redundant rustdoc target, deprecated
  `fetch_update` and a Unix-only test import on Windows. The follow-up removes
  the redundant target, preserves the saturating atomic update and memory
  ordering with an MSRV-compatible compare/exchange loop, and scopes the test
  reference to its Unix use.
- Clippy alone temporarily pins Rust 1.97.0 with warnings still denied because
  Rust 1.99 reports `double_must_use` in `async-trait` expansions; see the
  [upstream macro false-positive report](https://github.com/rust-lang/rust-clippy/issues/17529).
  All other stable CI jobs retain their current toolchain selection.
  New-head hosted CI remains pending; these local checks do not prove Windows,
  Linux, credentialed providers, core integration or host operator recovery.

### Committed PR snapshot validation (2026-10-01)

Runtime source: `f2ce258a1dbde6fb7c1ad6f8ae1a1bcfd14cef22`,
[PR #945](https://github.com/bitrouter/bitrouter/pull/945). Validation ran from a
clean isolated checkout, excluding unrelated route/model-discovery edits retained
in the development worktree. The validation-ledger follow-up changes docs only.

- `cargo nextest run -p bitrouter-orchestrator --all-features --status-level leak
  --final-status-level fail`: **102 passed, 0 skipped**, run
  `880f8c38-babf-4f03-92ff-eb0c8cd932b8`; no leak reported.
- `cargo nextest run --all-features --status-level leak --final-status-level fail`:
  **3677 passed, 22 skipped**, run `95e23a94-aac5-4ec1-a9ef-38c43aabd766`;
  no leak reported. The one-test difference from the development-worktree run
  below is the unrelated unpublished regression, not a removed runtime test.
- `cargo clippy --all-features --all-targets -- -D warnings`,
  `cargo fmt --all -- --check` and `git diff --check` pass.
  `cargo test --all-features --doc`: **5 passed, 1 ignored**; the orchestrator
  itself has zero doc tests. Build profiles use dev/test debug=0 and incremental=0.
- Plugin manifest JSON and eight internal-document local-link/fence checks pass.
  Existing linker unwind-size and dependency future-compatibility notices remain.
  This proves the local standalone runtime gate, not hosted CI, core integration,
  credentialed providers/ACP, other platforms or host operator recovery.

### Earlier standalone development-worktree acceptance (2026-10-01)

- Independent crate: `cargo nextest run -p bitrouter-orchestrator --all-features
  --status-level leak --final-status-level fail`: **102 passed, 0 skipped**, run
  `981b0bd9-2037-4ae5-bfa6-5dbc21137160`; no leak reported. The earlier standalone
  baseline was 91 tests; 11 added tests cover the recovery operation and native
  process fixture/matrix.
- Development-worktree regression: `cargo nextest run --all-features --status-level
  leak --final-status-level fail`: **3678 passed, 22 skipped**, run
  `41c34282-ca46-4409-a7f6-35e39ac616cf`; no leak reported. The skipped tests remain
  skipped under the existing configuration. This is local evidence.
- `cargo clippy --all-features --all-targets -- -D warnings`,
  `cargo fmt --all -- --check` and `git diff --check` pass.
  Independent `cargo test -p bitrouter-orchestrator --all-features --doc` succeeds
  with zero doc tests; workspace `cargo test --all-features --doc` passes
  **5 tests, 1 ignored**. The three updated runtime documents have valid local
  links and balanced Markdown fences. Existing macOS linker unwind-size and
  dependency future-compatibility notices remain; no new Rust/Clippy warnings.
- Recovery regressions preserve original Thread/Turn/Item identities, complete
  original tool results, paused FIFO, grants and key deduplication. Terminal
  legacy conversion preserves the original journal prefix. Known settled
  answers and passed/failed verification finish without another model/check;
  cancel intent preserves those verification facts. Between-step continuation
  retains cumulative budget and stops at exhausted bounds. Pending steering
  retains its target/key and is injected/bound once after recovery.
- Commit failure keeps the inspected Thread blocked. Exact committed-batch
  reread resolves lost ACK; caller detachment and concurrent duplicate acceptance
  produce one recovery batch. Empty Thread inspection releases a valid marker.
- A distinct OS process runs the actual native service and is killed after
  activation, ModelRequest, ToolIntent, ToolResult, RunCheckpoint, Settled and
  VerificationResult commits. A test-only durable journal image reopens the
  original active owner/facts. Every new runtime refuses recovery; root version,
  write evidence and verification counter remain unchanged. This proves local
  fail-closed runtime behavior at those windows, not production DB durability,
  forced takeover, shell isolation or investigated crash continuation.
- Stopped-source journal-prefix fixtures separately prove safe continuation and
  no replay. Their source service was actually joined; a copied prefix is not a
  process-crash proof. Unknown owner/effect/usage records remain blocked as the
  user requested, pending host investigation/proof.

### Private-review follow-up validation (2026-10-02)

- Five regressions cover pre-mutation tool rejection, conservative persistence
  failure evidence, corrected edits with stopped-owner transfer, bounded receipt
  paging/reader admission (including cold receipts and record limits), and
  once-only steering retries. Existing process-loss and interrupted-effect tests
  continue to pass.
- From the isolated follow-up checkout on macOS/Rust 1.97.0:
  `cargo nextest run --workspace --all-features --no-fail-fast` passed **3,682
  tests**, with **22 skipped**; workspace doc tests passed **5**, with **1
  ignored**; strict workspace/all-targets Clippy, formatting and diff checks passed.
- Builds used dev/test debug=0 and incremental=0. This is local evidence;
  credentialed-provider, Windows execution and production durability gates remain
  separate. Hosted CI for this follow-up is not included in these results.

### Earlier implementation evidence

- `cargo check -p bitrouter-orchestrator -p bitrouter` passed during integration.
- Orchestrator regressions pass. Includes injected
  admission/response/intent/result commit failures, no second effect after lost
  result commit, cancelled-response call settlement, provider-ID reuse, original
  ordering, observer resync, duplicate complete-call rejection before any effect,
  stable Item correlation, cancelled/truncated partial evidence and subprocess
  cleanup. R2 uses gates inside real file workers to prove overlap, both worker
  limits, out-of-order durable results with ordered context, edit barriers and
  joined workers after cancel/store failure. Verification approval/evidence,
  denial, permit-wait cancellation and interrupted process cleanup are covered.
- Ten new R3 core regressions pass: actual cross-Turn request history and reused
  provider IDs, FIFO activation with targeted withdrawal and preserved active
  workspace exclusion, no queued prompt injection, start/key conflicts, cancelled
  queue pause and explicit keyed resume without replay, resource waiting, owner
  and epoch/profile enforcement, durable cold key retries without workers,
  AllowEffects verification, keyed one-use approvals, atomic admission failure
  and durable unstarted queue retention on shutdown.
- Seven steering regressions cover in-flight modifying work and shared reads,
  fencing stale calls, ordered once-only context injection, retired/replaced
  approvals, same-Turn verification budgets, SDK requests already in flight,
  terminal not_applied decisions and lost application commits. The targeted
  orchestrator run passed **50 tests**, run
  `9158930e-d0b1-4d9a-ac97-c24ab01d0ab8`.
- Eight Thread observation regressions cover fixed pagination cutoff with newer
  Turns arriving, complete Item/call identity reconstruction after Task cache
  eviction, slow receiver resynchronization, unchanged approvals after detach,
  owner/epoch/grant enforcement, registration during an uncommitted admission,
  failed-commit snapshots without false cursor progress, bounded volatile tails,
  complete facts surviving later display-event failures, and late delta fencing.
  Store regressions verify count/byte pagination in memory and database, including
  database reopen and preserving the original event epoch. These ten new tests
  pass in the full suite below.
- Database key-index reopen/uniqueness coverage passes. Conflicting key reuse
  rolls back both an existing Thread version update and a new Thread creation;
  no partial acceptance facts remain. Core/storage targeted run:
  `89b97579-54a3-439e-9fa7-898fe52a01b7`, **14 passed**.
- Eight R4 reconstruction regressions cover committed admission/request/response/
  intent/result/settlement/terminal prefixes, already-modified files without
  replay, pending approvals/FIFO/cancelled pause, applied steering, partial model
  evidence and unknown usage, owner/epoch/grants/record limits, missing stable
  identity and exact verification invocation binding. Reader gates prove bounded
  concurrent scans, permit release and unrelated admission during storage waits.
  Invalid context cannot provide a settled context. Memory/database raw-record
  pagination adds two regressions for fixed cutoffs, count/byte bounds, reopen,
  oversized records and missing database rows. The targeted run passed **74
  tests**, run `80d853da-b63e-47a5-b608-8a236096af48`.
  Prefix fixtures are deterministic committed records; they do not prove actual
  native process-crash recovery, ownership transfer or effect resolution.
- Database tests pass for reopen/version conflicts and a committed record batch
  surviving abrupt loss of an independent writer process. This proves the store
  commit boundary, not native Turn recovery or unknown-effect resolution.
- Ownership regressions cover two independent services/connections competing,
  store-wide writer fencing, no raw-write bypass, retained clean-stop proofs,
  stale generation/retired instance rejection and no stop proof after injected
  commit failures. Legacy unfenced records block native execution and remain
  unchanged. An independent process commits its owner and a record, then is
  forcibly killed; peers stay blocked both before and after the kill/reopen.
  The targeted core/store/migration run passed **95 tests**, run
  `2c6724e7-4afe-428a-8804-0ce18bfde894`. The child writer is a test fixture;
  this proves conservative owner persistence, not native Turn/process adoption
  or lost-owner resolution. Seven new test entries include that fixture.
- Eleven new workspace test entries include the child lock fixture. Primitives
  verify held-lock conflicts, release-before-unlock ordering, persistent blocking
  after guard/process loss, immutable unknown claims under inspection, missing/
  invalid/oversized marker rejection and symlink rejection. Independent memory
  stores compete for one workspace; external release wakes a FIFO without a new
  RPC, and a rejected start leaves no key, prompt or request. A changed marker
  blocks an already-approved write. Cold inspection blocks independent fresh work
  and commits a recovery checkpoint for a local queue rather than indefinite
  capacity waiting. Release-preparation failure preserves a known completed write
  without publishing completion. A gated blocking acquisition proves disconnect
  cannot detach its job or let shutdown record false stopped proof. Targeted
  orchestrator run: **82 passed**, `c75ebc2c-c8bb-49c5-af74-8f7b4cba705a`.
  The child owns the real filesystem lock but is not a native Turn fixture;
  these tests do not prove full native process-crash recovery/continuation.
  The final v12 local-contract/core/store filter passed **84 tests**, run
  `9454da91-4b8c-4106-a985-512d7c4d2f68`; this filter is separate from the full suite.
- Seven new startup/index regressions verify fixed membership cutoffs with current
  head versions, count/byte admission, transaction rollback and database reopen,
  migration backfill and reconciliation of older-writer roots at owner transfer.
  Startup finds native Threads and legacy Tasks without whole-stream loads, hot
  context or model calls; paused FIFO remains recorded. Invalid call/result
  identity blocks fresh work before explicit load, including the first legacy
  submission, while an unrelated granted workspace runs. Cached-owner retries
  cannot bypass failed discovery. Root/record/metadata bounds fail closed. A later
  durable epoch selects its actual owner proof rather than the creation epoch.
  Core targeted run: **87 passed**, `f2cfa1e2-c6f9-48e9-92be-ef985b747e88`;
  database/migration filter: **26 passed**, `7b9f6195-266a-4de1-84e6-bff0b309b639`.
  These are core/storage tests, not native process-crash continuation evidence.
- Authority-refresh regression verifies that a later committed writer checkpoint
  preserves its recorded pause reason and stopped-owner proof without a worker,
  model request or durable mutation during load. History retains both original
  writer epochs while the response envelope uses the current serving epoch.
  The test fails on the prior code with the old pause reason, then passes after
  the projection fix. Red/green logs: `/tmp/bro-later-epoch-red.log`,
  `/tmp/bro-later-epoch-green.log`; green run
  `561f514f-42b8-4e3c-97b9-28e2e57a6118`.
- Four new regressions cover legacy approval/intent/complete prefixes, original
  IDs and record-position history, preserved counters, unchanged write mtime and
  no replay or private prompt leakage. Missing user identity and input/config mode
  mismatch withhold settled context. Database history survives reopen with count/
  byte/fixed-cutoff bounds; missing and oversized raw records fail visibly.
  A failed startup reconstruction stops growing context while still tracking the
  latest writer epoch, and cannot classify the root clean from a stopped owner.
  Initial core/database filter: **53 passed**, run
  `705f5ce0-76cf-44a8-913c-c5fe4a0e93cc`, `/tmp/bro-legacy-read-audit.log`.
  Final full-suite evidence below covers all four regressions together.
- Previous legacy-read `cargo nextest run --all-features`: **3667 passed, 22 skipped**, run
  `d3f9690e-990e-40d1-bf75-28bcd85886e6`, with no reported leak.
  Clippy all features/all targets, fmt and diff checks pass;
  `cargo test --all-features --doc`: **5 passed, 1 ignored**. Logs:
  `/tmp/bro-legacy-read-full-final-audit.log`,
  `/tmp/bro-legacy-read-clippy-audit.log`, `/tmp/bro-legacy-read-doc.log`.
  No new Clippy warnings remain. Existing dependency future-compatibility and
  macOS linker unwind-size notices remain.
  Local contract remains v13 with no new Thread/recovery RPC or CLI wiring.
  These are local core/storage checks, not durable conversion, real process-fault
  continuation, new C-phase acceptance or full MVP completion.
- Previous epoch-projection `cargo nextest run --all-features`: **3663 passed, 22 skipped**, run
  `ff9bb358-4880-40d5-a57a-327f405748f3`, with no leak reported.
  Clippy all features/all targets, fmt and diff checks passed;
  `cargo test --all-features --doc`: **5 passed, 1 ignored**. Logs:
  `/tmp/bro-later-epoch-full.log`, `/tmp/bro-later-epoch-clippy.log`,
  `/tmp/bro-later-epoch-doc.log`. Eight touched documentation files have valid
  local links and balanced fences. Local contract remains v13; no new Thread or
  recovery RPC was introduced. The existing macOS linker unwind-size and
  proc-macro-error2 future-compatibility notices remain. This is local regression
  evidence, not C0–C6 or full R4 acceptance.
- Previous startup-discovery `cargo nextest run --all-features`: **3662 passed, 22 skipped**, run
  `2ab9e5b5-914a-457f-b7c8-665237e61c9e`. It includes the final executing-epoch
  assertion. `cargo test --all-features --doc`: **5 passed, 1 ignored**.
  Clippy all features/all targets, fmt and diff checks pass. Logs:
  `/tmp/bro-startup-discovery-full-audit.log`,
  `/tmp/bro-startup-discovery-clippy-audit.log`,
  `/tmp/bro-startup-discovery-doc-final.log`. Local contract is now v13.
- Previous workspace-fencing `cargo nextest run --all-features`: **3655 passed, 22 skipped**, run
  `6b9952b4-e016-4383-af3c-b229e77e2d57`. Existing skip configuration was retained.
  Build used `CARGO_PROFILE_TEST_DEBUG=0`, `CARGO_PROFILE_DEV_DEBUG=0`, and
  `CARGO_INCREMENTAL=0` to constrain generated artifacts; test scope was unchanged.
- The preceding full run (`001ec885-4a80-43ae-8121-f34d691a3c92`)
  passed the same 3617 tests but reported one leaky test without identifying it
  at the selected output level. The audit rerun (`b986bbb9-c838-4514-942b-0089bd9c41af`)
  and the subsequent R3/R4 full suites enabled leak status output and reported no leaks.
  This intermittent anomaly has not been root-caused and
  remains evidence to investigate in R6 process/stress acceptance.
- `cargo test --all-features --doc`: **5 passed, 1 ignored**.
- `cargo clippy --all-features --all-targets`, `cargo fmt -- --check`, and `git diff --check`
  passed at the workspace-fencing checkpoint. Its logs: `/tmp/bro-workspace-fence-full-v12.log`,
  `/tmp/bro-workspace-fence-clippy-final3.log` and
  `/tmp/bro-workspace-fence-doc-v12.log`. No new Clippy warnings remain.
  The existing proc-macro-error2 future
  compatibility and macOS linker unwind-size notices remain.
- A disk-full build was repaired with package-scoped generated-artifact cleanup;
  a stale TUI artifact was rebuilt after cleaning that package. Source changes
  and other untracked work were preserved.
- The real PTY native TUI regression now approves edit and verification as
  separate identified requests, then checks completion, detach/reattach and
  cancellation. It passed in the latest suite. An earlier run correctly stopped
  at the newly required verification approval; its old test flow was updated.
  A new read-error assertion was corrected to the actual execution_status field.
- No hosted CI, credentialed provider, Windows or real ACP client acceptance is
  claimed. Full R3/R4/R5/R6 acceptance remains incomplete; these local checks do
  not prove the full MVP.

This record does not authorize automatic replay. R1 records are recovery inputs;
R4 must first establish old execution termination and effect status. Storage
failure may leave uncommitted cleanup information; last durable facts remain
the authority and the task stays blocked rather than claiming terminal success.

## Next implementation checkpoint

The core engineering spec is available at the inspected revision; its earlier
availability blocker is resolved. The user confirmed standalone runtime/crate
acceptance first, deferred client delivery and core integration, and kept unknown
owner/effect records blocked until host investigation/proof is implemented.
Do not migrate now or turn a document reference into implementation authority.

The standalone runtime now includes bounded reconstruction/discovery, fencing,
explicit terminal legacy conversion and safe same-Turn checkpoint continuation.
Deterministic stopped-source fixtures verify context/counter/result preservation;
native process faults verify that unretired owners remain blocked. Those are
distinct evidence: process disappearance never supplies a stopped proof, and
the tests do not prove investigated/forced crash recovery or production durability.

Next discuss the actual core/harness seam against the existing commit/launch
barriers and preserved runtime tests. Host work must supply recorded old-owner
termination/isolation and effect investigation before resolving unknown records.
R5 client controls and inbound ACP have independent authenticated delivery
acceptance. R6 still includes platform, provider, production backend and stress
evidence. The R ledger remains separate from the new C0–C6 ledger.

New identity fields deserialize as empty only for legacy records. The loader
marks missing identities invalid and withholds settled context. Legacy one-shot
Task conversion and continuation must preserve this boundary rather than
silently generate replacements or replay effects.

Passing the independent crate gate does not complete the broader product MVP,
core integration, unknown-effect operator recovery or inbound ACP delivery.
