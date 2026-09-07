# Native session evidence implementation plan

Status: implementation in progress. Baseline: `61dd7733`.
Foundation commit: `0af7f96a` on `feat/native-session-evidence`.

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
notifications stay in their unbound raw journal until their profile can be
established; durable rebinding/recovery is still required.

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

The application now creates the first task and attempt on a prompt whose native
session scope is confirmed. Subsequent prompts retain that identity across
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
process scopes to ACP operations, hooks, live Query caches and exact task ranges
still requires implementation; early ACP SDK journal records remain unbound.
Independent stage review passed after termination, persistent-gap, identity-gate
and ambient-field fixes. The workspace check ran 3,209 tests with 12 skipped;
Clippy, formatting, doctests, rustdoc and distribution checks also passed.

Still required: complete execution-relation parsing, native SDK lifecycle
rebinding; complete native query-lifetime recovery;
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
