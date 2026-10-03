# BRO native agent implementation record

**2026-10-02 contract update:** the approved
[Thread/Turn unification spec](BRO_THREAD_TURN_UNIFICATION_SPEC.md) supersedes
all Task compatibility, v13/v1 transport, duplicate completion events and
process-lifetime hot-capacity assumptions below. The body preserves its earlier
baseline. New validation belongs to the separate
[unification implementation record](BRO_THREAD_TURN_UNIFICATION_IMPLEMENTATION.md).
Core integration and lost-owner operator resolution remain separate tracks.

Design status (2026-10-01): the old native-agent and shared-session specs are
deprecated in favor of [BRO agent runtime MVP](BRO_AGENT_RUNTIME_SPEC.md).
That replacement is v0.2: concurrency, queue/steer, persistence and safe recovery
are confirmed MVP scope; implementation details remain under review. The
implementation and checks below describe the earlier runtime and do not verify
those capabilities or the proposed Thread/Turn/Item refactor.

Product 003 now gives orchestrator 004 v1.0 precedence for conflicting
core/harness responsibilities, interfaces and later stages. The
[migration handoff](BRO_AGENT_RUNTIME_HANDOFF.md) preserves the current runtime
inputs and separates them from unimplemented core/harness contracts. Neither
this historical evidence nor the runtime R-phase suite proves C-phase acceptance.

## Current runtime revision — 2026-09-30

This dated implementation baseline supersedes the persistence claims in the
historical phase notes below; it does not constrain the future v0.2 design.
`TaskService` in this baseline owns only process-local state and tracked
Agent workers. There is no task journal, fsync, startup replay, or restart
recovery. The existing database/router services keep their own responsibilities.

Local protocol v4 and the opt-in HTTP adapter expose the same runtime identity,
limits, snapshots, ordered assistant/shell deltas, and full pending approval
metadata. HTTP task operations require `X-Bro-Server-Instance`; `/observe`
streams SSE. CLI and TUI use subscriptions. The TUI can reconnect to the same
instance; a changed instance is shown as lost and is never resubmitted.
Snapshot cutoff and registration are atomic, with bounded historical catchup
and explicit snapshot resynchronization for a lagged observer.

Detach does not cancel a task. Cancel and termination resolve pending approvals
once. Server shutdown seals admission, cancels active tasks, and joins Agent,
shell, and configured verification cleanup. Active tasks are admission-limited;
terminal snapshots and caller-scoped idempotency expire together. Defaults and
operator-facing protocol details are in `docs/CLI.md`.

Verification for this revision:

- `cargo nextest run --all-features`: 3,582 passed, 22 skipped.
- `cargo test --doc --all-features`: 5 passed, 1 ignored.
- `cargo clippy --all-features --tests -- -D warnings`: passed.
- `cargo fmt -- --check` and `git diff --check`: passed.

The build used `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, and
`CARGO_INCREMENTAL=0` after the original debug build exhausted local disk.
Only generated artifacts in this checkout were cleaned with package-scoped
`cargo clean`; source and existing worktree changes were preserved.

Regressions cover ordered instance-local observation, approval one-use,
approval cancellation, bounded caches/admission, lagged observer snapshots,
terminal retention and idempotency eviction, and shutdown joining a verification
process. Separate-process tests exercise headless approval and live events,
authenticated HTTP SSE sharing the local runtime, stale-instance rejection,
and HTTP workspace isolation from locally registered paths. The PTY test
repeats detach nine times while awaiting input, then reattaches, approves,
verifies the edit, and exercises cancellation. Protocol tests keep control
`command_id` distinct from approval `request_id`, correlate replies, and retain
partial frame bytes when the input pump interrupts a stream read.

The earlier live-provider run is historical evidence for the earlier runtime;
it does not prove this refactor against a credentialed provider. This revision
has local mock-provider and real-terminal evidence. Hosted CI, Windows shell
execution, and a credentialed live-provider run were not performed.

## Historical phase implementation notes


This records exit evidence for the phases in
[`BRO_NATIVE_AGENT_SERVER_SPEC.md`](BRO_NATIVE_AGENT_SERVER_SPEC.md). It is an
implementation log; each section distinguishes implemented behavior from its
remaining validation gates.

## Reference consulted

Pi source revision: [`badlogic/pi-mono` `1b347794e2a630e4359f2584f4eea388145d0ddf`](https://github.com/badlogic/pi-mono/tree/1b347794e2a630e4359f2584f4eea388145d0ddf),
specifically `packages/agent/src/agent-loop.ts`. The design point used here is
one complete assistant turn followed by ordered tool results before the next
model request. BRO does not import Pi code or its extension/session format.

## P0 — routed native-call seam

Changed paths: `crates/bitrouter-sdk/src/app.rs`,
`crates/bitrouter-sdk/src/server.rs`,
`crates/bitrouter-sdk/src/language_model/pipeline.rs`, and focused tests in the
SDK. `App::execute_native` accepts a server-established caller and canonical
non-streaming prompt. It shares post-parse prompt preparation with HTTP and
uses the same pipeline for pre-resolution hooks, request checks, policy,
fallback, execution, and settlement. Native calls skip the SDK's
`ServerToolLoop`; BRO will execute its own client tool calls.

HTTP protocol parsing, inbound protocol preference, request header hints, and
header-sensitive prompt transforms remain HTTP-specific. Both HTTP and native
non-streaming turns finish accepted provider work and settlement after their
waiting client disconnects or cancels. A native turn supplies no HTTP headers, so transforms that
require them do not infer a Claude Code or Codex subscription identity. The
assembled server must establish a permitted `CallerContext`; this method does
not grant local authority from `server.skip_auth`.

Focused evidence: `cargo test -p bitrouter-sdk --features server --lib native_`
checks a permitted HTTP/native pair for the same transformed selector,
provider call, and recorded provider/model/effort/usage; a denied pair for zero
provider calls; and a native client-tool reply for no SDK server-tool execution.
The last test supplies only one mock provider turn, so an unintended second
turn from `ServerToolLoop` fails. This is deterministic SDK evidence, not a
credentialed provider or agent-server run.

Remaining risk at the P0 exit: the P1 loop, task service, process transport,
clients, and live provider path were not yet present. The native turn returns the
canonical model result; route and cost attribution are available at settlement
through the request ID and recorder, and P1/P2 must expose the needed view to
the task service without leaking provider credentials.

## P1 — deterministic native loop

Changed paths: new `crates/bitrouter-orchestrator/src/{agent,context,tools}.rs`
and its package/workspace manifests. `Agent` uses the P0 native call for each
complete model turn, records the assistant reply before executing any client
tool call, and appends one linked tool result per call. The first tools are
`read_file`, `apply_patch` (one exact text replacement, or new-file creation),
and `exec`. Calls run in reply order; a duplicate call ID cannot execute a
second effect. Malformed or unknown calls produce structured error results.
The context builder includes instructions and every recorded message and
rejects over-limit input without dropping a constraint or result.

Focused evidence: `cargo test -p bitrouter-orchestrator` covers a scripted
read → patch → shell check → final answer; ordered multiple calls; malformed,
unknown, and duplicate calls; cancellation before a new effect; and step,
time, estimated-spend, and context limits. The fixture uses SDK
`MockExecutor`, so it proves the engine behavior without a provider, process
transport, or persistence.

Remaining risk: the spend bound uses explicit estimated token rates, not the
host's settled actual charge. An in-flight model request continues to settle
after the agent stops waiting; provider-level cancellation is not yet wired.
The shell's working directory is not a sandbox, and an interrupted shell
command can have uncertain effects. P2 must journal that uncertainty rather
than replay it. The exact-replacement `apply_patch` schema is intentionally
small and may need a diff-based form after real coding tasks expose a need.

## P2 — persistent task service

Changed paths: `crates/bitrouter-orchestrator/src/service.rs`,
`apps/bitrouter/src/agent_local.rs`, and the host's task-service wiring. The
service journals accepted tasks, complete assistant turns, tool starts and
results, identified input, and terminal state as sequenced JSONL events. It
syncs records before exposing them. One active task owns a canonical workspace.
Restart marks unfinished tasks `interrupted`, with `unknown_effect` when a
tool may have started, and never replays a tool automatically. The app hosts an
owner-restricted, versioned local socket adapter; CLI and TUI clients use that
contract without owning a second agent loop.

Focused evidence: `cargo test -p bitrouter-orchestrator` covers durable
cursor replay, approval one-use and task binding, cancellation while awaiting
input, restart during a pending effect, and configured verification outcomes.
P1's scripted task also runs through service submission. This is still a mock
model fixture, not a separate-process or live-provider result.

Remaining risk: journal write failure stops execution but leaves an unfinished
task for conservative restart handling. The local socket authenticates its
owner through OS permissions; it is for a trusted single operator, not a
multi-user workspace sandbox.

## P3 — headless local client

Changed paths: `apps/bitrouter/src/main.rs`, `agent_local.rs`, `host.rs`, and
`tests/native_agent_process.rs`. `bro task run` starts or joins the local
server, checks the task contract version, submits a fixed-model task, renders
accepted/event/terminal NDJSON, and exits nonzero on an unsuccessful task. A
configured `--check` runs after the final answer and is recorded separately
from the agent's own shell actions. Existing `bro run <agent>` stays ACP owned.

Focused evidence: `cargo test -p bitrouter --test native_agent_process`
starts separate `bro` client/server processes against a wiremock provider.
The routed model reads a file, patches it, runs a shell check, saves its final
answer, and passes the configured verification command. The fixture observes
exactly two provider turns. Service tests supply reconnect/cursor and
cancel/approval evidence. Additional process tests exercise two clients
starting one server and rejecting an incompatible task-contract version.

Credentialed live-provider evidence: an isolated temporary home, config,
database, and workspace ran `bro task run` against the saved `supergrok`
provider with `x-ai/grok-4.5` on 2026-09-29. Task
`577b2144-19bc-4784-b24c-22b7a90f6972` read `note.txt` (`before`), used
`apply_patch` to replace it with `after`, ran `cat note.txt`, and delivered a
final answer. The configured verification command
`test "$(cat note.txt)" = after` exited 0, and the task journal ended in
`completed` with `verification: passed`. Four journaled model-turn request IDs
matched four settled `bro requests --provider supergrok` rows, all with
provider `supergrok`, model `grok-4.5`, no error, and provider-reported usage.
The request IDs were `51b6a1a9-84fa-4355-9add-be2a38a3581c`,
`6a485e3a-112b-42f5-8b79-3eb5c8431f70`,
`ea2b02fd-2353-4894-8397-f4107651a086`, and
`46f9c8ac-3042-4204-b7ca-182349836f00`.
The total reported usage was 3,790 prompt and 225 completion tokens, including
2,944 cache-read tokens. These rows were unpriced (`charge_status: unknown`),
so they prove the actual route and token provenance, not a charged amount.
The process fixture above remains separate local integration evidence.

## P4 — native interactive view

Changed paths: `crates/bitrouter-tui/src/native_agent.rs`,
`apps/bitrouter/src/native_code.rs`, and the bare `bro code` dispatch. The
existing TUI editor drives task submission; the renderer shows a projection
of task events and snapshots. `y`/`n` answers the service's identified pending
input, Ctrl-C requests cancellation, Ctrl-D detaches, and `--task-id` reattaches
with cursor replay. Explicit `bro code <agent>` stays on its ACP controller
and harness-native session path. A named remote context retains the
operations-only view.

Focused evidence: `cargo test -p bitrouter --test native_agent_tui` uses a
real PTY and separate server process. It observes a pending patch request,
approves it once, sees the final answer and passing verification, detaches and
reattaches to the same terminal task, then cancels a second pending patch
without changing the file. The model is wiremock, not a live provider.

Remaining risk: this first native view is smaller than the ACP conversation
UI. It has a bounded 500-line display projection; reconnecting replays the
persistent task journal. The former bare-Code ACP picker tests now exercise
the explicit ACP command after the cutover.

## P5 — opt-in task HTTP adapter

Changed paths: `crates/bitrouter-sdk/src/config/mod.rs`,
`apps/bitrouter/src/agent_api.rs`, `host.rs`, and the task service's
idempotency journal. `agent_api.enabled` defaults to false. When enabled, the
app requires a loopback bind, a dedicated bearer token environment reference,
and exact server-owned workspace paths. The HTTP handlers call the P2 service
for submit, read, events-after-cursor, identified input, and cancel. The
credential never follows inference `server.skip_auth` or control scopes.
Idempotency keys are journaled and restored across restart. An authenticated
native caller passes the assembled pipeline's auth hook without needing an
inference virtual key; policy and settlement still run.

Focused evidence: `cargo test -p bitrouter --test native_agent_process`
starts the HTTP listener and a local task client against one daemon. An
authenticated HTTP submission and the local socket read the same completed
task and cursor-ordered events. Unauthorized submission, a reused key with
different request content, and stale input are rejected. Service tests cover
idempotent replay across a journal restart. This is local integration with a
mock provider.

Remaining risk: the first HTTP release is for a trusted single operator via a
loopback listener or authenticated tunnel. It does not provide OS sandboxing,
remote TLS termination, tenant-specific workspace policy, or public exposure.

## P6 — integrated product gate

The native task flow is assembled in the existing `bro` executable and shared
daemon. Headless CLI, TUI, local task socket, and opt-in HTTP API use the same
task service and journal. The local fixture and PTY tests prove separate-process
client/server behavior; the live run above proves a credentialed routed coding
task. Existing ACP `bro run <agent>` and explicit `bro code <agent>` remain on
their harness path, and router/model CLI tests remain in the repository suite.
The skill, three plugin manifests, internal CLI reference, and config schema
were updated with the corresponding CLI/config changes. Hosted CI and
production deployment are outside this local evidence.

Final local checks after the last source edit: `cargo nextest run
--all-features` passed all 3,566 tests (22 skipped); `cargo clippy
--all-features` exited 0 with no new-code warnings; `cargo fmt -- --check`
passed; and `cargo run -p dist-helper -- check` confirmed the committed schema
and registry dist are current. The build reported an existing linker compact
unwind warning and a future-compatibility notice for `proc-macro-error2`.

## Follow-up — native streaming and the four coding tools

After the P0–P6 baseline, `App::execute_native_stream` was added alongside the
non-streaming native seam. It applies the same prompt preparation and routed
pipeline while skipping the SDK `ServerToolLoop`. BRO folds canonical stream
parts into a complete assistant turn, displays text deltas immediately, and
refuses to execute a tool call from a missing, length-limited, filtered, or
errored terminal response. Complete assistant messages and tool results remain
the durable journal records. A task snapshot carries a bounded, transient
`live` projection for the TUI; deltas do not increment the durable event cursor
or cause a journal fsync per token.

The model-facing tools are now `read`, `write`, `edit`, and `bash`. `read` supports
bounded, numbered UTF-8 text; image results are not supported by the present
tool-result wire. `write` creates directories and overwrites files. `edit`
accepts Pi-style `edits: [{oldText, newText}]`, matches every replacement
against one original snapshot, rejects ambiguity and overlap, preserves
untouched text, BOM, line endings, and permissions, and returns a bounded
unified diff. It intentionally does not use Pi's broader fuzzy punctuation or
whitespace normalization. `bash` sends bounded stdout/stderr chunks to the
transient live projection while retaining the complete bounded result and exit
status in the journal. Shell execution still has no OS workspace sandbox.
On Unix, shell commands use a dedicated process group and cancellation,
timeout, or shell exit kills remaining group members before pipe collection.
The local task socket contract is version 2 so an upgraded client rejects an
older daemon.

Focused evidence: `cargo test -p bitrouter-orchestrator --lib` passed 14 tests,
including multi-edit atomicity, ambiguity/overlap rejection, BOM/CRLF
preservation, symlink escape rejection, live output before shell exit, live
assistant delta ordering, and rejection of a length-truncated tool call.
`CARGO_INCREMENTAL=0 cargo test -p bitrouter --test native_agent_process
--test native_agent_tui` passed separate-process and PTY fixtures using SSE
provider responses. These are local fixtures; the earlier credentialed
live-provider run predates this follow-up and does not prove streamed behavior
with a live provider.

Final local checks after the source edits: `CARGO_INCREMENTAL=0 cargo nextest
run --all-features --status-level fail` passed 3,573 tests (22 skipped);
`CARGO_INCREMENTAL=0 cargo clippy --all-features` and `cargo fmt -- --check`
passed. No credentialed streamed-provider run was performed in this follow-up.

## Follow-up — inspection tools and read-only tasks

This follow-up consulted
[`earendil-works/pi` at `11894012`](https://github.com/earendil-works/pi/tree/11894012dd461232eb075bc890538b6866860a10),
especially its tool sets, `ls`, `find`, `grep`, and PowerShell shell selection.

The native coding tool set now also includes `ls`, `find`, and `grep` on all
platforms. `find` uses a glob; `grep` supports regex, literal text, case
selection, and a file glob. Both use Rust's `ignore` walker to respect
`.gitignore` without invoking or downloading `fd` or `rg`. They skip symlinks,
bound results and output, and check cancellation during traversal. `ls`
includes dotfiles. PowerShell is a separate Windows shell tool, with `pwsh`
preferred over `powershell.exe`; Unix exposes `bash`. Both shell paths retain
live output, timeout, cancellation, and bounded final results.

`--read-only` on native `bro task run` and `bro code`, or `read_only: true` on
the agent task API, selects a task-wide policy. The model sees only `read`,
`ls`, `find`, and `grep`, and the server rejects other tool names before the
approval path or dispatch. A read-only task cannot request a post-run
verification command because that command runs a shell. The accepted event
and task snapshot record the selected mode; idempotency fingerprints include
it. The local task socket contract is version 3, so an older daemon cannot
silently ignore the read-only request.

The read-only policy constrains BRO's native tools. It does not prevent
inspected file content from reaching the selected model. Windows PowerShell
behavior still needs a Windows runtime check; the local tests run on Unix.

Local evidence: `cargo test -p bitrouter-orchestrator` passed 17 tests;
the focused separate-process read-only CLI test passed after the accepted-record
mode was added; and `cargo nextest run --all-features --status-level fail`
passed 3,576 tests (22 skipped). Search
fixtures cover ignore rules, path limits, and symlink escapes; the agent
fixture proves disallowed effectful calls do not reach the approval channel.
