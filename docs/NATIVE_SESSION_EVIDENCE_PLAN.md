# Native session evidence implementation plan

Status: implementation in progress. Baseline: `61dd7733`.
Foundation commit: `0af7f96a` on `feat/native-session-evidence`.

## Current scope

| Layer | Implemented boundary | Remaining boundary |
|---|---|---|
| Collection | Both maintained controller launch paths persist registered native sources and ACP observations; restart reconciliation preserves evidence gaps. | Real pinned-runtime conformance and complete capability/version admission. |
| History | Source-local Codex and Claude context projection retains raw execution across supported compactions. Codex bounded fork ancestry is immutable. | Complete Claude independent-fork UUID remapping and unsupported native history formats. |
| Identity and relations | Native nodes, groups, processes, ACP attachments and candidate spawn/fork relations are distinct. Selected Claude SDK events can acquire verified per-observation process bindings. | Complete Query lifetime recovery, unmatched reset/rebinding cases and exact resumed-child execution ranges. |
| Task boundaries | A confirmed first prompt creates a task/attempt keyed by its ACP conversation, separately from native nodes; original prompt and response records commit with operation membership. | Explicit task/attempt switching, native execution membership and immutable per-attempt source cuts. |
| Settlement and artifacts | Prompt responses enter settling; immutable workspace baselines and candidate result checkpoints exist. | Native/background/child/request settlement, final artifacts and baseline-to-final attribution. |
| Evaluation | Manifest persistence/validation and request-set accounting primitives exist. | Production manifest construction, coding evaluation, authoritative Eval admission and human feedback. |
| TUI | Collection state is available in an application snapshot. | User-facing evaluation status, checkpoint feedback and task/attempt actions. |

These are feature-branch capabilities. A live collection snapshot is not a
complete evaluation sample, and an ACP response is not task-completion proof.

## Implementation checkpoint

Storage, source-local projection, recursive Codex history dependencies, native
spool ingestion, and durable ACP observation are implemented. The collector is
wired into both controller construction paths (`serve` and `launch_controlled`),
with background reconciliation and teardown draining. Targeted tests exercise
real database persistence, a fake native process, actual Node module resolution,
child transcript discovery and controller restart. Codex fork references now
bind immutable parent prefixes and record digests, including nested dependencies
after database reopen and deletion of parent files. Conflicting historical
prefixes remain ambiguous; missing or corrupt bound records cannot silently
rebind to a replacement file.

Claude per-session subprocess roots have separate journals and hook spools.
Preparation uses correlated operation ids and preserves the adapter's model
settings fallback. A cached fingerprint is not proof of native Query liveness:
an error or a profile change that could mean either reuse or recreation leaves
the scope unknown. Observations then stay unbound until a confirmed close or
a new controller connection resets the Query. A failed close is not reset
proof; the adapter may already have evicted it. These checks do not certify the
complete feature or the full native-runtime conformance matrix below.

The execution-facts stage additionally indexes native execution facts with their raw
record provenance and exposes a candidate graph in the collection snapshot.
It parses Codex turn and agent-call events, Claude prompt/stop hooks, and Claude
agent metadata sidecars, including nested agent transcript directories. Sidecar
binding verifies the owned transcript, file identity, stored prefix and native
session/agent identity. Spawn conflicts and cycles remain explicit gaps; a stop
hook or completed spawning tool never certifies child execution completion.
This graph is not task membership or a settlement decision. Independent review
and targeted tests cover raw/index replay, corruption, owner isolation, database
reopen, native event semantics, and metadata recovery. This does not complete
the full runtime conformance matrix.

The Claude SDK lifecycle stage opts into selected native extensions
through the existing adapter and adds application-selected notification fields
to the SDK observer. Confirmed session scopes index command states, native task
states, background task sets, results, runtime capabilities and conversation
resets separately. The original notification is forwarded after persistence;
the evidence view excludes configuration fields, result text and overlapping
cumulative cost counters. SDK task IDs are not agent transcript IDs. Early
notifications stay in their unbound raw journal. Individual observations can
subsequently acquire a verified native-process attachment as described below;
this does not rewrite their original scope or restore a live Query cache.

Controller recovery now continuously pages through owned source registrations
and their durable records. Original controller metadata verifies each historical
profile and spool; recovery does not reopen old journals for writing or restore
their live Query/session caches. It imports late spool records and recovers
identities from hooks already committed and retired by another controller.
Retained files rotate through bounded pages, and failures stay visible across
pages. Node publication and replay cursors survive cancellation together.
Corrupt registrations and recovery limits remain gaps while live collection
continues. Tests cover database reopen, profile separation, retired hooks,
concurrent controllers, cancellation, corrupt inventory cursors, and pagination.
A completed recovery sweep is not a task settlement decision.

The application now creates the first task and attempt on a prompt whose ACP
session profile scope is confirmed. Subsequent prompts retain that identity across
controller connections. Raw prompt observations, source cursors, operation
membership and attempt transitions commit together. Reads verify every recorded
prompt boundary against its owned source and original record, including completed
operations and the attempt origin. Overlapping RPCs remain collecting until all
recorded responses arrive; the final response only starts settling. A different
controller's outstanding RPC adds an explicit unobserved-response gap. It is not
declared completed, cancelled or currently executing by the replacement controller.
This does not recover native Query liveness or resolve a request committed before
an interrupted forward. The application snapshot exposes these attempts, but
native execution ranges, task switching and final settlement
are still required. Operation membership is bounded per attempt; it is not an
unlimited session-wide operation log.

Task-boundary tests cover both application harness paths, transactional rollback,
replay conflicts, corrupted original records, independent SQLite connections and
unanswered prompts after database reopen. A concurrent-read fixture verifies
stable snapshots on SQLite and has also been run against an isolated PostgreSQL
instance. The PostgreSQL fixture is opt-in with `BITROUTER_TEST_POSTGRES_URL` and
`cargo test -p bitrouter --lib --all-features
postgres_task_reads_keep_one_snapshot_during_concurrent_completion -- --ignored`;
the URL must name a dedicated test database. This is database concurrency
validation, not native harness runtime conformance.

Prompt boundaries now also bind content-addressed workspace artifacts. A confirmed
session's first prompt captures its actual working-file baseline, including dirty
tracked files and unignored new files. Later prompt responses capture candidate
result checkpoints; later edits or filesystem removal cannot mutate those stored
contents. Files are scoped to the Git repository containing the requested cwd.
The collector preserves binary bytes, tracked deletions and Unix executable modes;
symlinks store their target string without reading the target. These are filesystem
observations, not proof of exclusive authorship or final task completion.

Capture scope and native runtime exclusions are fixed to the original operation.
The actual connected SQLite filename and its sidecars are excluded even outside
BitRouter home. Both adapters' additional-root options are recognized as uncovered
scope, including possible Claude Query reuse. Missing cwd, non-Git workspaces,
submodules, sparse checkout, unsupported platform modes, unreadable files and
changed reads retain explicit gaps. Collection has a 30-second timeout, an 8 MiB
per-file limit, 16 MiB total raw content limit and 32 MiB serialized artifact limit.
Oversized serialization produces a partial artifact rather than rejecting a prompt.
These limits and two filesystem passes bound capture; they do not provide an
atomic filesystem snapshot. Ignored, untracked files are outside the file boundary.

Workspace objects are immutable and owner-scoped. New references are verified in
the raw-observation transaction; selecting a checkpoint rechecks its stored body.
Historical prompt validation retains exact raw boundaries without rereading every
old filesystem image. Tests cover old task serialization compatibility, concurrent
controllers with artifact references, profile uncertainty, external SQLite files,
additional roots, database reopen after workspace removal and corrupt artifacts.
An unavailable artifact keeps the valid task visible with an artifact-specific gap.
Final native settlement checkpoints, baseline-to-final deltas, shared-worktree
attribution, complete multi-root coverage and Eval manifests still require work.

Claude native CLI processes now have independent observation spools on Unix.
The maintained adapter's executable override starts a private BitRouter proxy;
its native argv, stdin/stdout bytes and exit code are preserved. A distinct
executable alias also forwards the adapter's auth status/logout calls directly
to the original CLI without capturing their output or changing ordinary
BitRouter commands in inherited MCP environments. Resolution uses
the adapter's SDK-local optional dependency, including Linux libc preference.
Explicit script executables and platforms without signal supervision retain
SDK ownership and expose a durable process-capture gap. Each process has a new
UUID even when the native session id is reused; process exit is separate from
command results, session idle and task completion.

The proxy records selected native lifecycle metadata before forwarding it,
including SDK versions, background-task ambient flags and conversation resets.
Oversized, malformed or incomplete frames continue downstream with explicit
capture gaps. Profile scopes are registered before creation; reused Queries
keep their original environment, while replacements use the newly prepared
spool. The proxy checks its actual native-root namespace before binding output.
Node discovery and fact extraction share process-id, source-file, sequence and
namespace validation. Invalid envelopes cannot authorize transcript collection.
These source gaps survive repeated reconciliation and historical replay.

Tests cover process-specific identities, pre-response native session discovery,
reset transcript discovery, database reopen, malformed envelopes, metadata
filtering, original argv/nonzero exit codes and SDK-relative Node resolution.
A real BitRouter binary fixture forwards TERM to an unresponsive test CLI,
then kills and reaps it before the SDK's five-second wrapper-kill deadline.
This is process/transport validation, not conformance against a real Claude CLI.
Abrupt wrapper death without a durable stop remains unknown. Correlating these
process scopes to prompt operations, hooks, live Query caches and exact task ranges
still requires implementation; early ACP SDK records without a matching native
event remain unbound.
Independent stage review passed after termination, persistent-gap, identity-gate
and ambient-field fixes. The workspace check ran 3,209 tests with 12 skipped;
Clippy, formatting, doctests, rustdoc and distribution checks also passed.

Process headers now carry a bounded reference to their committed creation
configuration. Preparation stores that configuration and its original ACP request
reference before forwarding; the private proxy retains only validated reference
fields and removes the marker before launching the native CLI. The snapshot
verifies the header, configuration, original request and both root registrations
against owned records. Controller, profile, spool, operation, parameters and
record digests must agree. Missing or invalid provenance leaves the process
visible with a gap. Historical recovery retains bindings after spool deletion.
The adapter can reuse saved creation parameters for several processes, so this
binding proves configuration origin, not the immediate cause of a restart,
current Query liveness, prompt execution membership or task completion.

Tests cover cross-controller and cross-profile references, mismatched original
parameters, corrupted header/configuration/request/registration records, owner
isolation, missing origins, and several processes using one saved configuration.
A real BitRouter binary test consumes the service's prepared environment,
captures a test CLI's actual output and verifies its database binding, including
removal of the private origin from the native child environment. This remains
transport validation with a test CLI, not real Claude runtime conformance.
Independent review passed after tightening the origin reference's generation
and coordinate-derived identity checks and adding the binary regression.
All 3,215 workspace tests passed with 12 skipped; Clippy, formatting, doctests,
rustdoc and distribution checks passed.

Lifecycle request and response boundaries are now indexed for both maintained
controllers. Each half references its exact original record and is committed
with live raw observations and source cursors; conflicting boundaries roll
back that transaction. Historical reconciliation can recover the two halves
from different profile journals in either order. Reads recheck both owned
records and their controller, harness and operation identities. Recovery does
not synthesize an outcome for an unanswered operation. A damaged derived
lifecycle boundary remains an explicit gap without blocking intact raw history
or later operations in the same journal; recovery never overwrites that boundary.

Claude process snapshots additionally expose the original creation operation's
verified response, including an ACP session id or rejection code. Load/resume
may use the requested id when the response omits it; new/fork require their own
returned id. This attachment remains tied to the original operation if saved
configuration later starts another process. A native conversation reset can
change the transcript id while the adapter keeps its ACP id; this attachment
alone does not rebind live Query caches or equate those identities. Missing
responses remain unobserved, and invalid response evidence remains a gap while
valid process configuration evidence stays visible.

Independent stage review passed after isolating historical lifecycle-index
failures. The new database regression deletes later indexes to force raw backfill
and verifies two controller restarts recover healthy sessions while preserving
the rejected, damaged boundary. All 3,222 workspace tests passed with 12 skipped;
one unrelated policy-lock test reported a nextest leak and passed an isolated
rerun without that report. Clippy, formatting, doctests, rustdoc and distribution
checks passed. These checks do not replace the outstanding native-runtime
conformance matrix.

The SDK identity stage introduced `native-evidence/2`. SDK facts retain the adapter's
ACP attachment separately from the message's native `session_id`, including
after a conversation reset. Missing native identity remains a gap. Versioned
fact ids permit replay into v2 without changing old v1 fact bytes or frozen
evidence objects. Both SDK and CLI command observations recognize `refused`.

SDK observations have a bounded, rotating application window, with an original
source-sequence cursor, inspected observation ranges and raw process ranges.
Early observations can bind to a
verified process/profile only when their native event UUID and all selected
metadata match a CLI record from the same controller. The process's original
configuration and available lifecycle response must agree. Multiple matching
processes remain ambiguous. Raw record validation supplies the candidate set;
missing derived indexes cannot make another process disappear. The original
invalid-field marker is checked before metadata reselection, and unmatched,
malformed or UUID-less observations stay explicit gaps.

Each window reads at most 128 ACP raw records and materializes one raw body at
a time. Candidate collection scans at most 100,000 raw process records and
retains the exact committed ranges. Verified origins exclude other controllers;
unknown origins remain in the inventory. Missing records, process/source limits
and unfinished recovery prevent a unique-process claim. Each collection epoch
captures SDK journal cuts before scanning any native spool. Completed registration
and directory sweeps remain at those cuts until all components finish, so their
different page lengths cannot permanently starve an observation. Late SDK records
wait for the next epoch and cannot bind against an older directory scan. A new
epoch starts directory enumeration from the beginning, including after failures.
Current journals come from registered roots even when an observer was cancelled
after committing raw evidence but before updating its source cache.

Spool import persists a monotonic observed byte extent before committing a
bounded record prefix. A pending file that disappears or becomes a non-regular
file no longer holds every SDK page in backlog. Its uncollected extent remains
an explicit inventory gap after database reopen; a missing or damaged extent
cannot certify a complete process candidate set. Later successful import may
cover that extent, but a shorter or replaced file never lowers the watermark.

Cursor advancement and snapshot publication occur together; later sweeps revisit
old observations for late or conflicting native copies. These windows are
replaceable status, not frozen attempt membership or Query-liveness evidence.
Durable per-attempt cuts still need to replace aggregate inventory budgets and
bound the cost of long-lived collection.

Independent stage review passed after tightening raw-candidate validation,
coordinated inventory cuts, cancellation recovery and durable missing-tail
coverage. A registered process with an observed extent but no imported raw
records remains an unknown candidate across restart; a process-source limit
remains visible after the excluded spool disappears. Regression tests cover
those cases, real multi-page spool inventories, late conflicting copies and
observer cancellation after raw commit. All 3,240 workspace tests passed with
12 skipped in the final low-concurrency run. Clippy, formatting, doctests,
rustdoc and distribution checks passed. The cancellation fixture uses an
independent read-only SQLite connection to inspect committed state while the
import future is paused. These tests do not certify real native-runtime
conformance, complete Query recovery, task membership or evaluation readiness.

The parser is now `native-evidence/3`. Each new fact pins its exact raw record
reference and source format; earlier facts retain their serialization and are
replayed separately. The Codex App Server tap also preserves `expectedTurnId`
on steering requests and the accepted `turnId` on responses. These native RPC
ids are distinct from the controller's ACP operation ids.

Application execution snapshots now expose Codex run bookends grouped by native
node and turn id. Starts, terminal outcomes and abort reasons retain original
record references. Anonymous rollout aborts never close an adjacent turn, agent
tool completion never closes a child run, and repeated child turns stay separate.
Conflicting terminal outcomes and reversed same-source bookends remain gaps.
Matching ids can join observations across sources; timestamps cannot order those
sources or establish execution membership.

Bookends also carry origin. Direct App Server events and rollouts with an intact,
non-fork initial metadata record can identify local execution. Copied or referenced
fork histories and missing metadata retain unverified bookends: an inherited
completion or synthetic fork abort cannot become the child's observed outcome,
even when a new directly observed turn reuses that id. A terminal without its
start retains a coverage gap. Run outcomes are observed status only; consumers
must check run, graph and history gaps, and still require task membership and
settlement evidence before evaluation. Exact attribution of local fork rollout
segments without direct events remains required.

ACP prompt-to-native input correlation is also still required. The inspected
Claude adapter 0.75.1 creates its own prompt UUID and its prompt response does
not carry that UUID. Session identity, input text and response timing do not
establish this link. The controller's operation boundary cannot yet select a
native command merely because it is the next one observed.

Independent review corrected copied-fork execution attribution, separated
unverified history from direct-event order checks, and removed quadratic work
over repeated bookends. Record reference validation runs once per extraction,
with the checked reference shared by that record's facts. Regressions cover
anonymous and named aborts, resumed child turns, inherited/synthetic fork
bookends, reused turn ids, conflicting outcomes, a large repeated-boundary set,
the run limit, and first v3 backfill after reopening a database with only v2
indexes. Old v1/v2 fact bytes remain unchanged and missing original terminal
records invalidate their derived evidence. The final workspace run passed all
3,250 tests with 12 skipped. Clippy, formatting, doctests, rustdoc and distribution
checks passed. An earlier run stopped on the unmodified CLI
version-probe timeout; both subsequent complete serial runs passed that test.
This remains format and transport coverage, not the full pinned native-runtime
conformance matrix or complete task execution attribution.

Application tasks now use `AcpSessionKey` rather than a native `NodeKey` as their
conversation identity. The first prompt no longer creates an assumed singleton
native membership. A Claude lifecycle response identifies only the adapter's
conversation; native hooks, CLI and scoped SDK evidence discover its transcript
nodes. The candidate graph follows a verified conversation-reset record to its
new native id without adding spawn/fork ancestry or assigning either execution
to a task. Missing native membership remains an explicit gap.

Task snapshots discover committed active-task objects independently of native
history discovery and live observer caches, then verify the original prompt
boundaries. Cancellation after the raw transaction cannot hide a task from its
own controller. Candidate discovery uses 16-row pages with owner, harness and
registered-namespace isolation. A corrupt row does not prevent later candidates
from being inspected. The total scan remains bounded by `MAX_GRAPH_ITEMS`, with
an explicit gap beyond the limit; it does not certify complete long-lived task
inventory and still needs task-scoped pagination for the full evaluation flow.

Pre-separation, unscored task objects remain readable through their original
digests and raw prompt provenance. Their assumed singleton native membership is
removed from the derived view; their task/attempt ids remain stable. Subsequent
transitions write the separated identity shape. Old and new active lookup keys
cannot coexist ambiguously. Legacy objects with extra members, manifest pointers,
ready state, mixed identities or damaged digests are rejected without rewriting
them. Immutable manifests are never migrated by this compatibility reader.

Independent review identified and corrected the commit-to-publication cancellation
window. The regression pauses the real observer after its database await, blocks
the state update, observes the committed prompt through an independent read-only
SQLite connection, then cancels and verifies same-controller task visibility.
Additional regressions cover three distinct ACP/reset native ids with a decoy
transcript, old database reopen and continuation, missing original evidence,
legacy-format rejection, conflicting lookup keys, and paged scope isolation.
The workspace run passed all 3,256 tests with 12 skipped. Clippy, formatting,
doctests, rustdoc and distribution checks passed. These fixtures do not certify
pinned native-runtime conformance or complete task execution attribution.

Still required: complete execution-relation parsing, remaining native SDK
rebinding cases without matching native events; complete native query-lifetime recovery;
capability/version gates; task membership and settlement; final workspace
checkpoints and deltas; authoritative
Eval admission/compilation; stable experiment identity; TUI feedback; complete
conformance, workspace checks, final review and PR delivery. Automatic first-attempt
creation is wired; explicit task/attempt switching, immutable native source cuts,
final artifact selection, dangling-RPC resolution, final settlement and score submission
are not. Manifest storage exists, but no application path yet compiles a complete
evaluation manifest. The live collection snapshot exposes history, candidate
execution facts, attempts and gaps; it is never optimization evidence by itself.

Recovery and the live candidate graph retain explicit resource limits. Task
scoping and durable per-attempt collection still need to replace aggregate
controller-lifetime candidate budgets before long-lived evaluation is complete.

The collection boundary is a BitRouter-controlled Codex or Claude Code session
and its registered native data root. Historical backfill is limited to that
session and explicit dependencies. It is not an automatic importer of every
standalone native session on the machine. Missing history, ambiguous fork cuts,
unsupported records and source replacement remain visible gaps. Supported
projection tests do not certify arbitrary native runtime versions, complete
Claude independent-fork UUID remapping, or filesystem rewind recovery.

## Outcome

Codex and Claude Code sessions driven through BitRouter automatically produce
durable, attributable evidence for evaluation. Compaction, child agents,
resumption and forks must not merge independent executions, duplicate spend,
or silently erase earlier evidence. A user or evaluator rates an immutable
attempt checkpoint, and the existing Eval exchange owns the result.

Native session IDs and lifecycle remain harness-owned. The application owns
an evidence index, not a replacement native session database. The SDK exposes
an observation contract; the routing core does not parse native transcripts.
The renderer receives evaluation state as data and keeps its synchronous,
application-independent boundary.

## Stages and completion evidence

1. **Storage and projection.** Add portable database migrations, owner-scoped
   native node/source/record storage, explicit lineage and immutable manifests.
   Preserve opaque raw records, source ordering and parser/producer versions.
   Model coverage separately for history, lineage, request attribution and
   artifacts. Tests prove idempotence, conflicts, tenant isolation, snapshot
   immutability, compaction and fork projection. Independent stage review.
2. **Native collection and reliable observation.** Implement Codex rollout
   collection (including checkpoint-bounded parent references) and App Server
   event ingestion; implement Claude transcript collection including every
   child transcript. Integrate durable full-envelope ACP observation before
   UI broadcast and capture lifecycle request/result boundaries. Persist
   runtime versions and capabilities. Support interrupted writes, reconnect
   replay, missing sources and reconciliation. Use published protocol/SDK
   references beside parsers. Independent stage review.
3. **Application and Eval integration.** Wire collection into the actual
   controlled-session launch/teardown paths. Bridge native identity to unique
   metered requests and existing decision references without heuristic claims
   of exact attribution. Freeze task attempts, source ranges and workspace
   artifacts. Expose session evidence and feedback through an app-owned
   interface and a minimal TUI action/status using existing renderer patterns.
   Submit coding observations and human feedback through existing Eval
   admission; incomplete evidence must not become a passing optimization
   sample. Independent stage review.
4. **Conformance and delivery.** Exercise the matrix below with deterministic
   native-format fixtures and actual process/transport integration. Verify
   compatibility against available pinned native runtimes without modifying
   user sessions. Update developer docs and shipped skill references. Run
   workspace all-feature tests, doctests, Clippy and formatting, plus affected
   distribution and architecture gates. Perform final independent review,
   resolve findings, commit and open a conventional-title pull request.

Every stage review must name missing behavior as well as implementation bugs.
The final audit maps this plan to production call sites and meaningful tests;
passing isolated parser tests does not prove automatic collection works.

## Invariants

- Native identities are namespaced by owner and source provenance. A
  controller connection is an observation instance, not a conversation key.
- Root grouping, spawn parent, inherited context and task membership are
  distinct relations. Text similarity and timestamps are not identity proof.
- A fork references an immutable source checkpoint. Later parent execution
  cannot enter its history. Inherited records are not new executions.
- Compaction changes effective context, not the raw audit log. Preserved
  messages and independent child compactions are represented explicitly.
- Resume/replay does not duplicate records. Content-block merging, event
  idempotency and usage accounting have separate keys.
- Raw source cursors advance only past durably stored complete records.
  Partial lines, source replacement, lag, unsupported versions and missing
  parents remain visible until reconciled.
- Manifests pin evidence, parser version, request/decision sets, artifacts and
  coverage. Later writes and revised ratings cannot mutate an earlier score.
- Costs are a union of actual requests, never a sum of overlapping parent and
  child counters. Missing prices or attribution remain unknown.
- Conversation rewind and filesystem rewind are separate. Evaluation never
  attributes a later working-tree state to an earlier checkpoint.
- Hooks and model-visible messages are evidence, not a verified claim that a
  coding task passed. Inconclusive evidence stays inconclusive.
- Auth/provider credentials are outside the native event capture allowlist.
  Stored transcript content remains local to the application unless an
  explicitly configured evaluator consumes a scoped evidence package.

## Required conformance matrix

| Case | Required evidence |
|---|---|
| Two compactions with preserved messages | Raw execution retained; effective chain correct; no duplicate executions |
| Parent and nested children compact independently | Complete node graph and per-node context ranges |
| Completed child resumes via a message | Same agent node, new execution range and immutable old evaluation |
| Fork at checkpoint; parent continues | Frozen inherited prefix; independent new execution and costs |
| Independent Claude fork vs fork subagent | Distinct session/agent relations, including remapped UUIDs |
| Codex paginated parent present/missing/unsupported | Correct bounded dependency traversal or explicit incomplete coverage |
| ACP replay overlaps native backfill | Source-local idempotence, explicit cross-source correlation, no duplicate charges or lost content blocks |
| Partial line/crash/move/mirror failure | Durable restart and reconciliation or visible gap |
| Clear/reset, cwd move and controller restart | Correct identity rebinding and counter reset scope |
| Identical prompts in independent sessions | Separate identities and evaluation/experiment units |
| Shared worktree, Bash edits and rewind | Checkpoint-specific artifact evidence and separate history transitions |
| Disabled/ephemeral/expired/unknown-fork source | Partial display allowed; no false complete evaluation sample |

## Source contracts

- [Codex App Server](https://learn.chatgpt.com/docs/app-server)
- [Codex hooks](https://learn.chatgpt.com/docs/hooks)
- [Claude session storage](https://code.claude.com/docs/en/agent-sdk/session-storage)
- [Claude sessions](https://code.claude.com/docs/en/agent-sdk/sessions)
- [Claude SDK types](https://code.claude.com/docs/en/agent-sdk/typescript)
- [Claude subagents](https://code.claude.com/docs/en/sub-agents)
- [Claude file checkpointing](https://code.claude.com/docs/en/agent-sdk/file-checkpointing)
- [ACP session setup](https://agentclientprotocol.com/protocol/v1/session-setup)

Claude collection initially uses the SDK's native transcript files and full
subagent directory, so it works with the existing pinned ACP adapter without
passing a JavaScript SessionStore object through JSON-RPC. The collector and
the evidence sink share the same append contract; future adapter-side mirrors
can supply that contract without changing Eval identity or storage semantics.

## Planning review decisions

- Codex native events need a production tap. The controlled launch installs an
  application-owned App Server stdio proxy through the adapter's documented
  `CODEX_PATH` override. It preserves an existing override, otherwise resolves
  Codex from the adapter process's PATH (including its bundled npm dependency),
  records the actual producer version, and forwards protocol bytes without
  owning sessions. Only an explicit conversation-event allowlist is persisted;
  provider/auth configuration is not transcript evidence. Native rollout
  reconciliation supplies historical and compaction dependency records.
- Both `chat` / `acp prompt` and the independent `acp serve` construction path
  install the evidence sink. A client subscription alone does not cover serve.
- Evidence moves through collecting, settling and ready/partial states.
  Prompt completion initiates reconciliation; source watermarks, outstanding
  children, request settlement and artifact capture determine readiness.
  Teardown drains evidence before and after stopping the child. A deadline
  produces explicit partial coverage, never a guessed complete checkpoint.
- A task starts explicitly through the application; automatic sessions begin
  with one task/attempt, with a user action to start another task or attempt.
  Feedback refers to the displayed immutable checkpoint even if the agent
  has since resumed. Later checkpoints select one effective eligible revision
  per attempt for optimization. An explicit feedback correction supersedes
  prior feedback without mutating its audit record.
- Native manifest coverage is checked in authoritative Eval admission and
  compilation, including direct submission paths. A dedicated built-in
  coding principal has bounded metrics; human identity is app-established.
  Generic existing Eval subjects remain compatible.
- The explicit attempt bridge supplies stable experiment identity across
  compact/resume and distinguishes new fork attempts. Missing reliable
  identity is ineligible for that experiment assignment, not a prompt hash.
- Collectors traverse registered native roots and explicitly related nodes
  only. They respect overridden data roots, bound dependency traversal and
  record cycles, owner mismatches, oversized records and persistence failure
  as observable failures. Source generations do not depend only on path or
  a shared prefix. Backpressure is bounded and cannot silently lose evidence.
- Raw bodies remain in local evidence storage. Eval evidence attributes carry
  redacted references/digests only. Tests distinguish per-source raw records
  from normalized logical actions and from unique accounting requests.
