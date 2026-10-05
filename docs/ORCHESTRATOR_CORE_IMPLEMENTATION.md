# Orchestrator Core Implementation Record

This record tracks implementation of [specification v1.0](ORCHESTRATOR_CORE_SPEC.md).
The specification remains the acceptance contract. An interface, unit test, or
mock-provider demonstration does not establish production integration.

## Baseline and integration

- The specification was frozen against main `d93ed73`.
- Existing native BRO work is in PR #945; the six-tool and durable-record work
  is stacked in PR #951 (integrated through `9981d7a2`; original core base
  `a58f40ca`). Its execution loop predates the core/harness
  split. Reuse its tools and storage through a harness adapter, with a single
  managed scheduler owning each session. Keep transparent external ACP separate.
- All changes remain on an isolated implementation branch. Do not change the
  dependency branches or treat their historical validation as this change's
  evidence.

## Work plan

| Stage | Work | Status / evidence |
| --- | --- | --- |
| C0 | Typed harness contract, capability negotiation, exact-byte checkpoint protocol, deterministic durable harness fixture | Implemented and independently reviewed; validation below |
| C1 | Root execution, shared prepared model pipeline, acknowledged step/output/tool/result barriers | Implemented and independently reviewed; validation below |
| C2 | Bounded concurrent child scheduling, durable collaboration, fair waits and cancellation | Implemented and independently reviewed; validation below |
| C3 | Context manifests and joint deterministic routing, hard feasibility and actual execution receipts | Implemented for the declared in-process paths, with independent reviews and the bounded exit evidence below; complete cross-stage acceptance remains open |
| C4 | Crash restoration, epoch/head reconciliation, queue/steer/cancel and uncertain effects | In progress: snapshot restoration, live reconnect, late provider evidence, root queue, steering, live observations, active-time cleanup, ownership release, cumulative activity handoff, frozen tool payload/body bounds, cleanup projection, recovery archives, logical artifact admission, durable capacity failure, canonical output admission, prospective receipt/delivery contributions and first recovery-observation archive reserves implemented; complete physical-storage/repeated-recovery cleanup guarantees and the explicit fault evidence below remain |
| C5 | Managed Responses and authenticated harness channel over the same core operations | In progress: durable response exchanges, atomic result continuation, virtual-key authentication, bounded registry, incremental HTTP/SSE projection and separate bounded WebSocket control lane connected to the service host; independent clients cover binding/release ACK loss, released-epoch restoration and read-only head queries during provider work; remote running-tool clock handoff, broader recovery/pressure conformance and acceptance remain |
| C6 | Production harness, independent client, real-provider and pressure conformance | Pending |
| Delivery | Independent stage reviews, complete acceptance audit, all-feature tests/doctests/clippy/fmt, PR and CI | Draft PR #956 submitted; stage reviews and validation recorded below; full acceptance audit, final independent review and final-head CI remain required |

Each stage receives an independent review. Findings and fixes are recorded with
the stage's actual validation commands. The final independent review checks the
whole contract rather than only the new diff.

## Acceptance evidence

The complete A01–A23 matrix is **not yet proven**. The stage records below contain
bounded executable evidence; final acceptance must cover every scenario, including
remote/in-process parity, real concurrency, durable output and effect barriers,
crash/ACK-loss recovery, caller isolation, provider accounting, and actual
production-harness integration. No completed subset narrows the original scope.

### Open acceptance work after the cross-transport increment

The source audit distinguishes existing admission from missing proof. New tool
intents and later model prompts already pass through `prepare_checkpoint`,
`capacity::check` and `artifact_storage::check` before acceptance/dispatch.
`artifact_storage::recovery_archive_reservation_precedes_tool_dispatch` covers
low-quota rejection without tool execution. These are not unimplemented
admission APIs. New tool-batch overflow after a committed model outcome now has
failure/terminal ACK-loss and existing-tool cleanup evidence, described below.
Accepted tool results now have focused next-model-attempt capacity failure,
ACK-loss and cleanup evidence. These cases commit the prepared history and
model plan before rejecting the attempt; they do not establish every earlier
context or prompt-construction admission boundary.

The process fixture covers checkpoint boundaries with quiescent handoff and
in-flight incomplete provider HTTP bodies with explicitly scripted trusted
activity input. The latter retains uncertain spend without applying partial
calls, but does not establish production activity measurement. Repeated full
Running observations now reach logical archive-quota refusal and still allow
first essential stopped/unknown observations and full uncertain/definite
outcomes. Result-before-observation ordering also has artifact/checkpoint
pressure and ACK-loss evidence. Five deterministic storage-boundary fault cases
now cover absent/partial/complete archive staging and checkpoint failure before
or after append, preserving byte identity and cleanup. Queue/steer/cancel
combinations, other recovery growth boundaries and production storage-full
behavior remain independent conformance work. The injected staging limits and
atomic failures do not establish physical leases, OS ENOSPC behavior or
unlimited future handoff growth.

Production storage reservations, historical checkpoint retention/reclamation,
workspace read/write/shell barriers, actual provider cost reconciliation and
physical memory measurements need evidence from the relevant implementation.
Core-owned buffer accounting does not constrain arbitrary allocations made
inside a custom executor or callback before it returns.

Remote Running-tool restoration remains explicitly unsupported. Independent
monotonic clocks and uncertain one-way transport delay cannot turn message
arrival or RTT into an exact activity stop time. A usable bridge requires a
trusted measurement domain, complete activity coverage through replacement
entry and a lifecycle drain; unknown coverage must still reject restoration.
Quiescent restoration and in-process measured handoff do not establish that
remote capability. These gaps do not narrow A01–A23 or replace A23 with mocks.

## Validation environment

Initial local validation used Rust 1.95.0, aarch64-apple-darwin. Subsequent
increments identify their compiler below; current CI uses stable Rust. Nextest
is available. Builds use
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`
to keep the workspace within available disk capacity; these change build
artifacts, not test selection or runtime assertions.

## C0 evidence

The `core::protocol` DTOs define the v1 harness operations, manifests, limits,
errors and receipts. `core::checkpoint` validates exact payload bytes, epoch and
scope, ordered appends, complete artifact references, and matching durable ACKs.
`CommitGate` exposes only committed dispatch eligibility. The deterministic
fixture atomically retains checkpoint batches and their original ACKs; it is not
a production persistence implementation.

- `cargo test -p bitrouter-orchestrator --test core_contract`: 18 passed.
- `cargo nextest run -p bitrouter-orchestrator --all-features`: 54 passed.
- `cargo test -p bitrouter-orchestrator --doc --all-features`: passed (no doctests).
- `cargo clippy -p bitrouter-orchestrator --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, and `git diff --check`: passed.
- Independent review found and fixed four defects before stage completion:
  divergent reconnect heads now fail closed; historical exact retransmissions
  return the retained ACK without rewinding the head; base64 and the full wire
  envelope count toward pending-output limits; inline materials carry media type.
- Regression tests also reject unknown control fields and ensure old epochs
  cannot use retained acknowledgements to bypass fencing.
- A follow-up independent review found no remaining C0 blockers.
- Strict clippy on Rust 1.95 exposed an inherited collapsible match in the
  legacy context validator; collapsed the match guard without changing behavior.

These tests establish the contract primitives only. They do not yet prove A01–A23
end-to-end execution, scheduling, recovery or production integration.

## C1 evidence

`core::session::CoreSession` implements new-session binding, idempotent root
input, committed model plans and attempts, complete output validation, harness
tool dispatch/results, explicit verification, and a separate terminal commit.
State inspection remains available while a harness ACK is pending. The SDK's
controlled native entry point uses the shared preparation, route selection,
fallback, execution, and settlement machinery. Plans exclude provider credentials;
each fallback has its own admitted attempt and complete outcome record.

- `cargo nextest run -p bitrouter-sdk -p bitrouter-orchestrator --all-features`:
  1119 passed, 2 skipped. This includes 21 core execution tests.
- `cargo test -p bitrouter-orchestrator -p bitrouter-sdk --doc --all-features`:
  5 SDK doctests passed, 1 ignored; orchestrator has no doctests.
- `cargo clippy -p bitrouter-orchestrator -p bitrouter-sdk --all-targets
  --all-features -- -D warnings`, `cargo fmt --all -- --check`, and
  `git diff --check`: passed.
- A real SDK HTTP executor connects to a loopback provider fixture, emits a
  file-read call, consumes an actual temporary-file result, and finishes the
  task. This is protocol integration evidence, not a credentialed live-provider
  or production-harness run.
- Independent review findings were fixed with targeted regressions: abandoned
  drivers cannot duplicate model steps; observer callbacks precede final model
  admission; disconnect interrupts ACK waits while allowing usage settlement;
  invalid verification is rejected before acceptance; tool delivery survives
  cancellation of its caller; stale or corrupt ACKs report unknown commit
  status; unknown effects prevent subsequent tool dispatch; frozen tool sets,
  tool choice, and parallel-call constraints govern output validation.
- Fallback admission also checks the accumulated active-time bound. Unsettled
  attempts take precedence over resource-limit terminal transitions.
- Follow-up independent review found no remaining C1 blockers.

This stage exposes the in-process root primitive only. It does not establish
child scheduling, context selection, crash restoration, Responses/channel
integration, or production-harness conformance. Those remain C2–C6 work; A01–A23
remain unproven as complete end-to-end acceptance scenarios.

## C2 preparation

Execution state now belongs to `AgentState` and `AgentTurn`: each agent owns
its history and context revision; each turn owns its model steps, tool
invocations and provisional answer. `RootRun` retains the shared budget and
committed overall outcome. The root uses the same agent-indexed model-step
function that child scheduling will use. This is a state refactor, not evidence
of concurrent child execution.

- `cargo nextest run -p bitrouter-orchestrator --all-features`: 76 passed,
  including 22 core execution tests. The additional regression starts a second
  root run, preserves the stable agent context, and verifies fresh run/turn/tool
  identities and reset shared counters.
- Strict all-target/all-feature crate clippy, formatting and diff checks passed.
- Independent review found no root-execution regression. Before enabling
  children, remove the remaining root-only event attribution, derive aggregate
  run status from all agents, and apply admission to both the run and selected
  agent. The current driver deliberately still accepts only root execution.

The scheduling work below completes the next part of C2 after this preparation.

## C2 scheduling evidence

One session driver schedules bounded concurrent agent steps through the shared
SDK path. A single collaboration dispatcher handles the seven core-owned
actions. Model calls retain their provider call/result pairing; runtime actions
have explicit runtime provenance. Agents retain history across FIFO follow-ups,
including later root runs. Result delivery records the assigning agent, while
structural ownership controls subtree cancellation. A provisional final answer
waits for descendant and assigned work, pending mail, tools, and verification.

Runtime waits are durable observations keyed by their accepted operation IDs in
the snapshot. Their completion is committed without creating a model tool call
or occupying a model slot. Model waits park their agent, release its slot, and
wake on relevant state/mail changes or deadlines. Wait observations identify
queued assignments rather than returning an earlier completed turn's answer.

- `cargo test -p bitrouter-orchestrator --test core_execution`: 36 passed.
- `cargo nextest run -p bitrouter-sdk -p bitrouter-orchestrator --all-features`:
  1134 passed, 2 skipped.
- SDK/orchestrator all-feature doctests: 5 passed, 1 ignored.
- Strict all-target/all-feature SDK/orchestrator clippy, formatting and diff
  checks passed.
- Concurrent held providers demonstrate two actual overlapping child calls,
  a bounded peak of two, a root scheduling opportunity, correct attribution,
  and final incorporation of child evidence. Active provider time measures the
  union of intervals while each attempt retains its own elapsed-time receipt.
- A one-slot model-driven spawn/delegate/wait run sends only workspace reads
  to the harness. Other regressions cover mailbox replay, queued follow-ups,
  wait deadlines and tool-result wakeups while another model is still running,
  graph bounds, explicit wait cycles, and implicit assignment/join cycles.
- Cancellation regressions retain billed provider output while discarding new
  effects, wait for dispatched tool cleanup, preserve paired history before
  interruption, archive child results to cancelled recipients even with a full
  mailbox, and serialize cleanup against another agent's pending output ACK.
- Independent reviews found and fixed stale transition races, stale verification
  reuse, cross-run queue selection, mailbox-capacity cancellation deadlock,
  timer/admission retry loops, and dependency cycles through idle ancestors.
  Follow-up read-only review found no remaining concrete C2 blocker.

This stage establishes in-process scheduling mechanics, not complete joint
context feasibility, crash restoration, remote transport parity, production
harness integration, or tool-active-time accounting from harness status signals.
Reuse policy hardening and decision receipts remain C3; restoration and durable
root queue/steering remain C4. C5/C6 and the full A01–A23 audit remain pending.

## C3 signal and material groundwork

Revisioned harness signals are bound to the authenticated session and harness.
Material inventories retain immutable version/digest/provenance identities,
including removed versions. Required references are pinned to accepted work;
missing content is requested only after a matching checkpoint ACK, then verified
before model dispatch. Inventory changes invalidate stale plans and unstarted
effects at safe boundaries. Every step retains its admitted manifest and
materials, so later permission changes cannot reinterpret emitted calls.

- SDK/orchestrator all-feature nextest: 1145 passed, 2 skipped, including
  47 core execution tests. All-feature doctests: 5 passed, 1 ignored.
- Strict all-target/all-feature SDK/orchestrator clippy, formatting, and diff
  checks passed.
- Regressions cover wrong signal scope, immutable identity conflicts, aggregate
  quotas, missing/corrupt/stale material, removed and restored inventory,
  material-send disconnect, output-bound changes, tool removal, and required
  context changes while a provider outcome awaits acknowledgement.
- Independent review found and repaired stale verification admission, result
  bounds taken from a later manifest, conflicting artifact IDs, resolved-ref
  refetch deadlock, noninterruptible material sends, and late tool observations
  overwriting current workspace facts. Duplicate result delivery under a new
  operation ID now also preserves newer workspace signals.
- Follow-up review confirmed the repairs. This is groundwork, not completion
  of C3: joint context/model decisions, reuse feasibility and execution receipts
  still require implementation and review. C4–C6 and full acceptance remain open.

## C3 prepared-plan records and manual constraints

Each model step now commits an ordered source context manifest and paired input
history. The prepared provider plan must retain required system instructions and
the ordered committed messages, with structurally valid call/result pairs. Its
`RoutingDecision`, `DecisionApplied`, and per-attempt `ExecutionReceipt` share a
decision identity. A rejected or stale prepared context is durably recorded
before the SDK returns the admission failure. Model preparation failures before
a candidate plan exists still take the earlier failed-step path.

Fixed model mode resolves the requested alias and provider chain but skips the
named model selector. Policy mode selects once. Both preserve explicit caller
effort and its provenance. A controlled continuation requires a known matching
authoritative effort, including an explicit provider default; a conflicting or
unknown constraint cannot dispatch. Ordinary uncontrolled HTTP/native selection
retains its existing behavior. The host continuation adapter propagates this
authoritative fact from its sealed continuation record.

Fresh children preserve applicable user instructions. Inherited children copy
paired source history without old injected material bodies, so they resolve the
current material inventory. Assignments freeze the assigning agent's required
instructions, including new root constraints for workers receiving later FIFO
follow-ups. An explicit child model overrides policy selection.

- SDK/orchestrator all-feature nextest: 1153 passed, 2 skipped, including
  51 core execution tests.
- App all-feature continuation regression: 122 passed (122 selected by
  `test(continuation)`). SDK/orchestrator doctests: 5 passed, 1 ignored.
- Strict all-target/all-feature clippy for the app, SDK and orchestrator,
  formatting, and diff checks passed. Rust 1.95 exposed four inherited match
  and Boolean-expression lints in app/TUI dependencies; equivalent expression
  simplifications also passed 253 TUI tests and 11 selected app session/wire
  tests, and received an independent read-only review.
- Regressions cover preparation deleting instructions/history, inherited
  material replacement, fresh-context instruction retention, cross-run
  instruction propagation, decision/application/attempt joins, unknown usage,
  raw totals without cache evidence, fixed/policy selector call counts,
  policy-effort provenance, and fixed continuation effort compatibility.
- Independent SDK review found and repaired false caller ownership of policy
  effort, route-hook model attribution changes, and a fixed-mode continuation
  constraint bypass. Independent context review found and repaired missing
  follow-up instructions and raw totals being misclassified as cache evidence.
  Follow-up reviews found no remaining blocker within this groundwork scope.

This is still not full C3 acceptance. Current records describe continuation of
the selected agent; worker candidate enumeration, strict reuse feasibility,
capacity/output-allowance checks, reconstruction, priced cost receipts and
protocol-specific cache observations remain pending. Token estimates/capacity,
cost and cache observations are explicitly unknown in these records. The full
C4–C6 delivery, A01–A23 audit, final workspace checks, PR and CI remain required.

## C3 worker allocation and retained context provenance

Explicit spawn and bounded delegation now commit context candidate evaluations.
Delegation prefers eligible idle workers in stable agent-ID order, then a fresh
child. An exact target has no fallback; independent/fresh work cannot reuse a
worker. Fixed context mode permits these explicit operations just as auto mode
does. A selected allocation is referenced by the assignment, turn and subsequent
model decisions. Context eligibility does not assert model/token feasibility.

Reuse requires an exact nonempty scope, settled calls and effects, no queued work
or unconsumed mail, no dependent work/cycle, paired history, known matching
workspace versions, and matching permissions, tool manifests and material
identities. Retained context keeps cumulative provenance across turns, inherited
history, messages, wait observations and harness results. A newer model step
cannot certify older evidence as newly observed. Tool-result workspace revisions
remain per-result evidence rather than overwriting the current harness signal.

Candidate material checks include dependencies retained by the proposed worker,
not just the assigning task. An unavailable optional historical dependency can
exclude reuse while leaving the fresh candidate feasible. Queued activation and
first-step admission check effective work requirements again, including IDs
pinned by later signals. Stale activation is durably rejected without a model
attempt; the assigning agent receives the failed task outcome.

Rejected allocation decisions and their operation receipts are durable. Runtime
errors then report `commit_status=committed` for the rejection record, without
accepting or retargeting the requested work. Same-ID retries return the original
error; clients can inspect the operation and allocation IDs and must use the new
committed revision for a different operation. Model-originated rejections retain
the same decision evidence and a paired failed collaboration result.

- SDK/orchestrator all-feature nextest: 1164 passed, 2 skipped, including
  62 core execution tests.
- SDK/orchestrator all-feature doctests: 5 passed, 1 ignored. Strict
  all-target/all-feature SDK/orchestrator clippy, formatting and diff checks
  passed.
- Regressions execute real worker reuse in both routing modes; stable ties and
  exclusion of unavailable earlier candidates; fresh fallback for unknown or
  changed facts, isolation and busy workers; exact-target rejection/replay;
  context/decision/attempt joins; and reservation invalidation after signals.
- Independent review found and fixed missing tool-result and wait-conclusion
  provenance, availability checks that omitted retained optional dependencies,
  and activation checks that used the original input instead of requirements
  pinned by later signals. Each has an executable regression. Follow-up review
  found no remaining concrete blocker within this segment.

Workspace validation at this checkpoint is incomplete. The default-concurrency
`cargo nextest run --all-features` stopped after 1600 of 3686 selected tests:
1595 passed and five background CLI tests hit their 15-second deadlines. All
eight tests selected by `test(background::)` subsequently passed with one test
thread. This narrows the issue to investigate but does not establish a passing
full run. Workspace all-feature doctests passed (5 passed, 1 ignored); a bounded
full nextest run and workspace clippy still need to finish before final delivery.

C3 remains open: provider capability/capacity and output-reservation checks,
infeasible continuation reconstruction, priced cost receipts and protocol-aware
cache/continuation observations still require implementation and review. C4–C6,
the full A01–A23 audit, final workspace validation, PR and CI remain required.

## C3 provider constraints and output reservations

Managed TaskInput accepts a positive `max_output_tokens`; omission reserves
4096 tokens per model step. Child tasks inherit that bound. Shared preparation
cannot remove or rewrite it. Each concrete provider model may declare independent
`token_limits.max_input_tokens`, `max_output_tokens`, and `context_window`
values in configuration. Input-only limits do not imply a combined window.
Registry entries without these facts remain unknown.

The route and its capability/limit facts are captured from the same configuration
snapshot. Core records each candidate's rejection and unverified constraints,
then admits an ordered subset of the shared pipeline's frozen chain. No model
reselection occurs. Rejected candidates do not consume provider attempts; actual
fallbacks retain their original route indices in intents and receipts. Known
protocol incompatibilities, insufficient output limits, empty input capacity, and
an output reservation exhausting the entire combined window reject a candidate.
An empty admitted set produces a durable rejected decision before execution.
Catalog capability declarations are positive observations, not an exhaustive
inventory; the cross-entry audit below corrected the earlier missing-capability
rejection to retain explicit uncertainty.

Executor/provider declarations identify routes that cannot preserve output
limits. The Codex subscription shaper removes this parameter, so managed core
rejects that route before authentication or a model attempt. HTTP execution also
checks the final request body after provider shaping and authentication, including
authentication retries. The actual outbound adapter validates the limit; built-in
wires reuse their parsers and Antigravity checks its nested request envelope.
Custom adapters without validation support are rejected during admission.
Ordinary, uncontrolled API calls retain their existing behavior.

- SDK/orchestrator/providers all-feature nextest: 1425 passed, 2 skipped,
  including 67 core execution tests.
- Workspace all-feature doctests: 5 passed, 1 ignored. Strict workspace
  all-target/all-feature clippy, formatting, diff and generated-distribution
  checks passed.
- Regressions exercise non-contiguous fallback indices, no dispatch for rejected
  candidates, zero/default/explicit reservations, child inheritance, preparation
  mutation, config reload between route resolution and plan admission, all four
  built-in wire renderers, authentication body mutation, actual Codex shaping and
  Antigravity's final envelope.
- Independent reviews found and repaired config-snapshot mixing and Codex output
  limit removal. Follow-up SDK review found a custom-protocol compatibility gap;
  adapter-owned validation and an Antigravity regression repaired it. Final
  read-only reviews found no remaining concrete blocker within this segment.

Input token counts still have no tokenizer and remain explicitly unknown,
including when provider limits are known. A route admitted with unknown input
size is not a verified fit. Complete input/capability/protocol feasibility,
infeasible-context reconstruction, priced cost and protocol-aware cache receipts
remain C3 work. C4–C6, A01–A23, the final independent audit, PR and CI remain open.

The preceding full-workspace reruns were not clean: a two-thread run passed
3683/3686 selected tests with three CLI/PTY timeouts; two serial runs each passed
3685/3686, with a background lifecycle timeout in the first and a SIGINT fixture
connection timeout in the second. The background fixture now includes daemon
log tails and last observed run state on timeouts without changing its deadline
or success assertions; this diagnostic change also passed independent review.
These intermittent failures have not been explained or fixed. The subsequent
full serial run at `47086108` completed: 3697 passed, 22 skipped in 330.272 seconds.
That is evidence of a passing workspace run, not an explanation of the preceding
intermittent failures. Final delivery still requires validation of its final tree.

## C3 provider input counting and joint token capacity

Provider models can explicitly configure `input_token_counting: responses`.
Only managed native execution invokes it; API wire compatibility does not imply
the provider implements a counting endpoint. Without a configured counter the
input count remains unknown. A configured counter must succeed for that candidate
to be admitted; unavailable or malformed counts cannot downgrade to unknown fit.

The HTTP path uses the actual Responses renderer, provider model, static
transport authentication and final request body. It sends the documented input
fields, including instructions, tools, structured output and origin-bound prior
response references, to `/responses/input_tokens`. Unknown input extensions,
mutable provider conversations, automatic truncation, unsupported protocols and
dynamic authentication fail this counting candidate. Counting is bounded by a
30-second total request deadline and a 16-KiB response limit. Upstream error text
is not saved in routing records; only controlled error categories are retained.

The receipt commits the final generation body, endpoint and authenticated headers
to a digest. Immediately before generation the HTTP executor checks this digest
against the count selected for that exact attempt. Changed content, credentials
or semantic headers invalidate it; another candidate's successful count cannot
authorize a failed counter. Input counts are route-specific observations, separate
from model-independent context size estimates, generated usage and cache evidence.

Core commits count intent before contacting the provider and count outcome before
another count or plan admission. Source revisions, permissions, cancellation and
the durable dispatch gate are checked again. Counting consumes active wall time
but not a generation attempt. It does not fabricate generated tokens or a zero
price. Successful counts enforce both independent input limits and the combined
input-plus-output window, including exact boundaries and arithmetic overflow.
Model/effort selection and provider candidate identities stay frozen throughout.

- SDK/orchestrator/providers all-feature nextest: 1435 passed, 2 skipped,
  including 74 core execution tests.
- Real HTTP loopback regressions exercise count payloads, concrete provider
  filtering, a successful Responses terminal, malformed/unavailable counts,
  exact combined-window limits, Unicode/tool input, durable ACK ordering,
  permission changes, cancellation, disconnect, commit failure and active-time
  exhaustion. They are provider-protocol evidence, not a live-provider run.
- Independent review found and repaired raw diagnostic leakage and count proof
  sharing across candidates. Added regressions exercise error redaction, request
  mutation, credentials/organization headers and failed-candidate isolation.
- Core read-only review found no concrete blocker in the count barriers or
  capacity checks. SDK follow-up review confirmed both repairs with no remaining
  concrete blocker in this segment. Strict all-target/all-feature workspace
  clippy, formatting, diff and generated-distribution checks passed. Workspace
  all-feature doctests passed (5 passed, 1 ignored). The serial workspace nextest
  run at `1f37fe64` finished with 3705 passed, two failed and 22 skipped in
  374.316 seconds. Both failures were background CLI startup/exit deadlines:
  `background_load_and_resume_use_advertised_native_capabilities` and
  `background_permissions_default_to_ask_and_explicit_modes_win`. The daemon log
  tails were empty. Their cause remains unresolved; this is not a passing full
  run. The preceding 3697-test passing run covered the output-reservation
  checkpoint, not this input-counting implementation.

C3 still requires infeasible-context reconstruction, complete capability and
continuation feasibility, priced receipts and protocol-aware cache accounting.
Other provider counters remain unknown unless a supported counter is explicitly
configured; these observations must not become assertions of universal fit.
C4–C6, the full A01–A23 audit, final independent review, PR and CI remain required.


## C3 explicit optional-history reconstruction

A rejected capacity plan can now create one deterministic candidate
from the same required materials and retained history. `TaskInput.discardable_history`
is an explicit, task-scoped caller constraint, bound by SHA-256 to the complete
canonical history before that task. Only listed settled assistant/tool messages
can be removed. Missing or stale declarations retain all evidence; a finished
work unit or an unrelated required document never authorizes deletion. Children,
worker assignments and follow-ups do not inherit the declaration.

At this checkpoint, the candidate retained user/system instructions, all current
work, required material bodies and preparation additions, and validated complete
call/result pairing. The production revalidation change below additionally
rejects preparation-added messages without a dependency contract. Core records the rejected source step, a reconstruction receipt, source
history digest and a new context revision/step. Its CPU work consumes active time;
checkpoint ACK waiting does not. No second count or provider attempt begins until
the reconstruction checkpoint is acknowledged. A still-infeasible candidate is
rejected without another reconstruction loop or model reselection.

The shared SDK accepts only a strict ordered message subset, keeps generation
parameters/model/effort/provider candidates frozen, reruns the original request
checker bindings and performs fresh provider counts. A checker denial prevents
both the second count and generation. At `b966120c`, private continuation,
App ingress prompt transforms, and all four mutable preparation/route-hook groups
disabled automatic reconstruction. That checkpoint proved only a hook-free
embedding; production hook support is implemented in the next segment.

- SDK/orchestrator all-feature nextest: 1192 passed, 2 skipped, including
  80 core execution tests.
- Regressions cover explicit reduction and preserved required/current evidence;
  a task depending on the prior plan/artifact with only an unrelated README;
  stale, unordered, out-of-range or instruction-removing declarations; broken
  call pairs; fixed mode; missing material; mandatory context still too large;
  reconstruction ACK ordering, disconnect and commit failure; App transforms
  adding dependencies on prior evidence; task fingerprinting
  and child/follow-up isolation; frozen checker denial before egress; one model
  selection; invalid rewrites; mutable hooks and continuation restrictions.
- Independent SDK review confirmed the frozen checker repair and found no
  remaining concrete blocker in that segment. Core follow-up review confirmed
  task-scoped declarations and the ACK boundary.
  An additional audit found App ingress transforms outside the SDK hook groups;
  core now refuses reconstruction when those transforms are installed. Review
  of that repair confirmed the boundary without remaining concrete blockers.
  The broad serial workspace run completed with 3715 passed, one background
  CLI timeout and 22 skipped in 302.986 seconds. It preceded the final App
  transform guard. The failing test was again
  `background_load_and_resume_use_advertised_native_capabilities`, with an empty
  daemon log at its unchanged 15-second deadline. The cause remains unresolved.
  After the App-transform repair, SDK/orchestrator nextest again passed all
  1192 tests (2 skipped). Strict all-target/all-feature workspace clippy,
  workspace all-feature doctests (5 passed, 1 ignored), formatting and diff
  checks passed. These do not resolve the background CLI failure above.

At that checkpoint, C3 remained open for production hook revalidation, full
protocol and continuation feasibility, priced receipts and cache accounting.
The typed in-process declaration does not establish a remote context-inventory
API or the C5 wire mapping. C4–C6, the complete A01–A23 audit, final independent
review, final-tree workspace checks, PR and CI remain required.


## C3 production context revalidation

Preparation hooks and route hooks now have separate read-only revalidation
contracts. Unknown implementations reject reconstructed context by default.
Production authentication rereads the credential and checks the frozen caller,
policy and route-principal bindings. Policy revalidation retains the ingress
selector and policy binding while rereading live permissions and usage. Session
identity, evolution admission fences, judge ownership, continuation state and
server-tool declarations are checked without repeating registration or selection.
The model, effort, tools, generation parameters and provider chain remain frozen.

The App validates every ingress transform, even for a custom native embedding
control. Each transform validates once, after durable validation admission, and
must explicitly permit removal of its dependencies. Built-in model/alias
transforms preserve their frozen decisions. Core rejects preparation-added
messages because the caller's history declaration does not describe their
semantic dependencies. Provider-private continuation remains unsupported for
this reconstruction strategy.

`context.rebuild` records a candidate step without replacing the agent's visible
history. `context.validation.intent` must be acknowledged before any transform,
read-only preparation hook, frozen request checker or route guard runs. Checks
retain their original order: request checkers precede route guards. Each guard
rechecks the live source, cancellation, dispatch and budget gate. Actual guard
work consumes shared active time; waiting for another checkpoint between guards
does not. The validation report records only a controlled error category.

`context.validation.outcome` stores the report and, only when validation allows
and the source and live dispatch gate still permit it, activates the candidate.
Activation and its `applied` flag become visible together after ACK. Denial,
provisional permission/cancellation blocks and disconnect cannot replace the
old history. Runtime child inheritance also ignores unactivated candidates.
Fresh counts and final admission require both acknowledged Allow and activation.

- A shipped `assemble::build_app` + `CoreSession` integration uses a real HTTP
  executor against a loopback Responses fixture. Two tasks trigger one rebuild,
  fresh input counting, preserved required material and successful generation.
  Metering retains exactly two logical request rows. This establishes production
  App wiring with an embedding harness fixture, not a live-provider or production
  harness acceptance run.
- Regressions cover frozen selector/model/effort/provider chain, policy reload,
  key rebinding/revocation, checker-before-route ordering, transform enforcement
  for custom controls, all three checkpoint barriers, guard denial, signal and
  cancellation changes, disconnect, inter-guard ACK waits, next-task history,
  provisional dispatch blocks and child inheritance of pending candidates.
- Independent reviews found and repaired duplicated ungated transform validation,
  ACK waits charged as active time, route guards running before request checks,
  rejected history leaking into later tasks/children, and activation racing a
  provisional dispatch block.

- The 19 targeted revalidation regressions passed. The final serial workspace
  all-feature nextest run passed all 3726 selected tests, with 22 skipped, in
  287.303 seconds. The previously intermittent background CLI tests passed in
  this run; their earlier timeout cause remains unexplained.
- Both independent follow-up reviews found no remaining concrete blocker in
  this segment. Those reviews were static; the test evidence above is separate.
- Strict all-target/all-feature workspace clippy, workspace all-feature
  doctests (5 passed, 1 ignored), formatting and diff checks passed.

C3 still requires full protocol/continuation feasibility and priced/cache
receipts. C4–C6, A01–A23, final independent audit, final workspace validation,
PR and CI remain open.

## C3 configured token estimates and cache evidence

Each actual managed provider attempt now retains its execution provider/model,
an optional configured token estimate, and explicit raw cache observations. The
production App supplies the same immutable pricing-table Arc used by its
MeteringRecorder. The estimator uses the existing tier resolution, normalized
token buckets, deterministic rounding and price-version calculation. It performs
no I/O and writes no additional metering row. The selected route and actual
execution identity remain distinct, including custom executors and fallbacks.

Configured estimates retain integer micro-USD, usage origin, normalized counters,
effective rates, pricing identity and version. Unknown usage, missing required
raw totals, invalid bucket/shape/counter evidence, unpriced nonzero buckets,
unconfigured prices, excessive token counts, invalid rates and saturated charges
remain unknown. Explicitly configured zero rates can produce a known zero. A
failed attempt without usage remains unknown even when request settlement uses
its legacy zero-usage rejection normalization.

Cache evidence uses explicit fields in Responses, Chat Completions, Messages and
Generate Content usage. Missing cache counters are not observed zeros. Messages
streaming input/cache fields use cumulative delta values when present, including
zero, and otherwise initial values; final output totals must come from the final
delta. Malformed wrappers/details are rejected. Cache counters must match their
canonical buckets and do not establish KV transfer, reuse savings or final bills.

The root run accumulates known token estimates and unknown outcomes in the same
acknowledged checkpoint as each unique attempt receipt. Admitted intents without
acknowledged outcomes stay pending. Child/follow-up turn replacement cannot erase
earlier costs; a new root run starts a new subtotal. Overflow makes the subtotal
unknown, and legacy snapshots without this ledger retain `None`. A complete token
estimate requires known outcomes for every admitted attempt. It must not be
added to request settlement or presented as a full run invoice.

These are model-token estimates only. Provider tools, counter fees, storage,
account adjustments, preparation/integration charges and final reconciliation
are outside this subtotal. Existing preparation/count/rebuild/validation records
retain elapsed work separately; missing monetary evidence is not a zero charge.
This segment does not establish full A09/A21 acceptance, monetary reservation
policy, C4 recovery or C6 production conformance.

Independent review found and repaired an initial-output fallback that could
misinterpret incomplete Messages streaming usage as a known final zero. Both
accounting reviews found no remaining concrete blocker in this segment after
that repair. Reviews were read-only; executable validation is recorded below.

- The 17 targeted regressions passed, including a production App/HTTP-executor
  fixture that matches native cost, normalized usage and pricing version against
  the existing two metering rows. Rejected reconstruction candidates do not add
  attempts or a second settlement record. This remains loopback-provider and
  embedding-harness evidence, not live-provider or production-harness acceptance.
- Core regressions cover pending/failed outcome ACKs, repeated drive, unknown
  failed fallback costs, retained child/follow-up totals, root-run reset, legacy
  absence, interrupted child outcomes and subtotal overflow. SDK/product tests
  cover raw protocol counters, incomplete Messages output, malformed containers,
  actual pricing identities, context tiers, missing usage/prices and invalid
  rates/counts/amounts.
- `cargo nextest run --workspace --all-features --build-jobs 2 --test-threads 1
  --no-fail-fast`: 3740 passed, 22 skipped, in 303.963 seconds. The earlier
  intermittent background CLI timeout did not reproduce; its cause remains
  unresolved.
- Strict all-target/all-feature workspace clippy, workspace all-feature
  doctests (5 passed, 1 ignored), formatting and diff checks passed.

The next C3 segment must freeze shared adapter prompt compatibility into route
feasibility, reject known lossy protocol conversions and automatic truncation,
and bind or reject provider-private native continuation with redacted adjustment
receipts. Full accounting acceptance, C4–C6, A01–A23, final independent audit,
final workspace validation, PR and CI remain open.


## C3 shared protocol feasibility and final wire integrity

Every controlled native call now freezes the shared executor's local protocol
assessment into each `NativeRoute.protocol_validation`. The HTTP implementation
uses the actual adapter, target model and provider's pure body normalization,
without authentication, project discovery or provider I/O. Unknown custom
executors remain explicitly unverified; custom HTTP adapters must implement
managed validation. A rebuilt candidate receives a fresh assessment without
model/effort reselection or changing the original candidate indices.

Known lossy candidates are rejected before input counting or model admission.
The SDK and core share the exact predicate for required count receipts, and
skipped candidates cannot carry an invented count outcome. A custom embedding
control cannot admit a route the executor rejected. Core persists controlled
rejection categories and skips rejected candidates while retaining their
positions in the frozen chain.

Checks cover typed generation controls, provider-specific tools and tool
identities, nested tool media/file references, mixed tool messages, unsupported
reasoning replay and approval history. Gemini schema conversions that change
the canonical schema are not certified as equivalent. Nested media also enters
the existing shared capability derivation. These checks describe the actual
current adapters, not universal provider support for every such input shape.

Managed HTTP forbids automatic truncation, mutable provider conversations,
unacknowledged provider compaction, unbound Responses continuation, Gemini
private cached-content references and upstream multi-agent scheduling. This
applies even without an input counter. Complete native-entry continuation
binding, redacted adjustment receipts and broader continuation acceptance remain
unfinished; this segment does not claim them.

The immutable expected wire body is captured before asynchronous provider
shaping. After authentication and on every retry, the adapter checks the entire
final semantic body against that baseline, in addition to existing output-limit,
credential-authority and exact input-count guards. Claude Code shares its pure
identity-prefix/extension normalization with ordinary shaping; Antigravity
checks its inner request and outer model while leaving project discovery to its
existing authentication path. Ordinary uncontrolled requests keep their prior
behavior.

Independent review found and repaired Messages mixed-tool text loss, Responses
custom-call false rejection, Gemini schema changes and private cache references,
paired denial loss, cross-protocol tool identity loss, unsupported reasoning
replay and incomplete final-body control coverage. The end-to-end fixture and
review also exposed the core still requiring counts for rejected candidates;
the shared count predicate fixed that admission mismatch. Final read-only
review found no remaining concrete blocker in this segment.

- Ten targeted regressions passed. The real HTTP/core fixture covers a rejected
  configured-count candidate followed by a viable Responses candidate, with and
  without counting, and all-candidate rejection for automatic truncation. It
  asserts no count/attempt for the rejected candidate and retained route index.
- Other regressions cover media capabilities, renderer failures, custom tool
  identity, reasoning/approval loss, schema changes, custom-control admission,
  private context, final authentication mutations and both provider wrappers.
- Final-tree `cargo nextest run --workspace --all-features --build-jobs 2
  --test-threads 1 --no-fail-fast`: 3749 passed, 22 skipped, in 210.887 seconds.
- Strict all-target/all-feature workspace clippy, workspace all-feature
  doctests (5 passed, 1 ignored), formatting, diff and generated-distribution
  checks passed.

Workspace validation exposed an inherited fixture isolation defect. Eight test
configurations put `inherit_defaults: false` under `registry`, where the parser
ignores it. Cold fixture homes therefore still allowed remote registry downloads
with a 15-second per-request timeout, competing with the 15-second CLI readiness
assertions. They now explicitly set `registry.enabled: false`. All changes are
confined to test fixtures; production behavior, deadlines and success assertions
are unchanged. Independent review confirmed the field placement and that no
same-shaped fixture error remains. A first full rerun passed 3749 tests after
the two initial configuration fixes; final verification includes all eight.
This identifies and removes an actual external-network dependency, rather than
using longer deadlines to tolerate it. It does not retroactively prove the
cause of every historical intermittent failure.

C3 remains in progress for complete continuation feasibility/adjustment evidence
and full accounting acceptance. Private-item binding must cover Anthropic
signed/redacted thinking and Gemini thought signatures retained in agent history,
including provider/model/account changes and inherited child contexts, in addition
to Responses continuation handles. C4–C6, A01–A23, final independent audit,
final-tree workspace validation, PR and CI remain open.


### Remaining C3 acceptance audit

A separate read-only audit confirmed that the token subtotal is still only part
of the run-cost contract. The cost-work inventory below now retains preparation,
input counting, validation, reconstruction, model and workspace-tool exposure,
including durable unknown costs. Remaining integration sources and actual
monetary evidence still need coverage. Reported and reconciled charges need independent,
idempotently correlated evidence alongside estimates; provider-reported token
usage is not a reported charge, and the values must not be added together as
independent bills. The existing authoritative settlement path should supply this
evidence rather than introducing another charging implementation.

The audit also identified missing direct native/HTTP evidence for A09; the
cross-entry checks below address that part. Requested effort remains a request
fact, not a provider observation. A hard monetary limit is not currently offered;
conservative monetary reservations become mandatory if that policy is enabled.

## C3 native and ordinary HTTP execution parity

The production App now has direct cross-entry integration coverage. A controlled
native call and an independently authored ordinary HTTP request use the same
assembled pipeline and loopback provider. The gateway uses actual HTTP sockets.
Sixteen inbound/outbound combinations cover Responses, Chat Completions, Messages
and Generate Content for the common non-streaming function-tool subset. Tests
compare complete provider request bodies and independently assert instruction
roles, content/tool wrappers, array cardinality, schema, sampling controls,
output limits, actual model or URL, response text and token totals. Each request
has its own settlement identity; normalized usage, token estimate and pricing
version match the native receipt without extra settlement rows.

A richer Responses fixture covers a bound router and request checker, caller
overrides of defaults, strict tools/output schema, effort and parallel-tool
constraints, and an actual failed first provider followed by a differently named
serving model. Captured wire models and attempt routes are bound to the frozen
plan, not inferred from the mock's response. Context-tier prices and raw cache
counters match request settlement. Missing usage and failed attempt costs remain
unknown. Checker denial matches the public native/HTTP error contract and reaches
neither provider. These fixtures use synthetic local callers, not authentication
or cross-session isolation evidence.

The comparison exposed a core interpretation defect: the shared configured
catalog intentionally treats capabilities as positive observations, whereas core
had treated any nonempty list as exhaustive. An omitted requirement now remains
`required_capability_unknown` in the decision's unverified constraints. Empty
inventories retain `capabilities_unknown`. Known adapter incompatibilities,
unsupported output reservations, failed configured counts and exceeded limits
still reject. Unknown catalog facts cannot cause reconstruction; the obsolete
capability-rejection reconstruction branch was removed. A real App/CoreSession
regression completes fallback with partial capability metadata, records the
uncertainty, performs no rebuild and preserves unknown failed-attempt expense.

Independent review confirmed the metadata semantics and found two potential
false-positive test patterns: matching bodies without checking the actual model,
and matching text without roles/tool wrappers. Both were replaced with independent
wire expectations. Final read-only reviews found no remaining concrete issue in
this segment; executable validation is separate below.

The new fixtures retain a separate temporary configuration/runtime directory.
Their first ordinary Responses runs exposed the default no-path assembly using
the repository working directory for lazy continuation state, which violated
the existing HTTP matrix's clean-directory precondition. Supplying the temporary
config path fixes fixture isolation without changing production defaults or
weakening those assertions. The matrix also checks that its execution leaves no
installation/continuation files in the repository. Independent review confirmed
the temporary directories remain alive through requests and settlement.

- Final targeted integration binary: 5 passed, including 4 new tests and the
  existing reconstruction integration. The new matrix executes all 16 protocol
  combinations, in addition to the richer Responses and core regressions.
- Final-tree `cargo nextest run --workspace --all-features --build-jobs 2
  --test-threads 1 --no-fail-fast`: 3753 passed, 22 skipped, in 209.033 seconds.
- Strict all-target/all-feature workspace clippy, all-feature workspace
  doctests (5 passed, 1 ignored), formatting and diff checks passed.
- The final run passed the existing clean-directory checks and left no runtime
  state files in the repository. The earlier pre-isolation run failed those
  preconditions; it is not included as passing evidence.

This establishes non-streaming model-entry parity for these fixtures. It does not
establish managed remote transport, private continuation or full monetary
accounting. C3's continuation/cost contracts, C4–C6, the full A01–A23 audit, final
independent review, PR and CI remain open.

## C3 authentication proof prerequisites

`AppliedAuth` now retains a transient commitment to the authenticated URL,
credential header names and values, and known account-selection headers. The
executor keeps that wrapper through provider header rules, tracing and request-id
injection, then revalidates it before dispatch. Replacing one Bearer or API key
with another no longer preserves an earlier proof merely because the scheme is
unchanged. OpenAI organization/project, Anthropic workspace, ChatGPT account and
Google user-project header changes also invalidate the proof, including removal
and duplicate values. The commitment is private, uses length-delimited inputs and
is neither serialized nor added to debug output.

Stable principal identity remains separate from this per-request check. An
authority-aware OAuth applier can install a refreshed access token and produce a
new request proof with the same principal. Ordinary trace and compatibility
headers do not invalidate it. The executor clears stale request authority before
authentication and records a replacement only after final body, output-limit and
count checks succeed. This request-local field is still not an independent proof
that a generation attempt succeeded; signed-output sealing must use provenance
owned by that successful attempt, rather than a count or earlier fallback.

The Anthropic Platform API applier now derives its authority from the same
resolved API key that it installs, preserving stored-key precedence, inline key
overrides and account-label lookup. Its route-time proof uses the actual
`x-api-key` scheme even when the target's configured scheme differs. Replacing a
key under the same account label changes the proof. Explicit Anthropic workspace
selection remains unverified until that scope is atomically bound; ordinary
Messages requests continue to work. Claude Code and Antigravity OAuth still
provide no verified principal and require further work. An account label alone
is not account identity.

Configured account selectors are resolved before authentication, using actual
allowed inbound values or their configured defaults. Their stable commitment is
folded into the existing credential authority at both routing and dispatch;
ordinary Responses calls and same-scope continuation remain available. Changing
organization or project cannot reuse an earlier native continuation. Without
inbound context, a passthrough scope remains unknown instead of guessing its
default. The executor also rejects an applier changing a configured scope. The
counter follows the same header order and compares its final semantic headers
with the generation request. Existing unscoped records cannot acquire new scoped
authority retrospectively.

This strengthens existing continuation authentication as well as preparing for
signed-history binding. It is not a claim of complete provider-private history
support. Anthropic signed/redacted
thinking, Gemini signatures, child inheritance, model switches, output sealing
and adjustment receipts remain part of the open C3 continuation work.

Independent review found two missing OpenAI scope headers and a test that did
not independently inspect the actual API key on wire; both were corrected.
Targeted regressions cover same-scheme credential replacement, destination and
scope mutation, stable principal identity after refresh, no upstream dispatch
on invalid proof, and a real Messages pipeline with the production Anthropic
applier and loopback provider.

Review also caught that merely invalidating headers added after authentication
would reject first Responses calls with configured scope. The stable scope
binding above fixes that regression rather than weakening the proof. A production
App fixture exercises actual gateway HTTP calls with both static defaults and
passthrough headers: initial calls succeed, matching continuation substitutes the
native handle, independent organization/project changes make zero upstream
requests, and a fresh request under a new scope succeeds.

Final independent review found no remaining blocker in this authentication/scope
segment after the count-path wiring and ordinary unproven-applier guard were
fixed. This review does not cover completion of the remaining C3–C6 work.

- Final-tree workspace nextest: 3763 passed, 22 skipped, in 209.015 seconds.
- Strict all-target/all-feature workspace clippy, all-feature workspace
  doctests, formatting and diff checks passed.
- The earlier 3760-test pass preceded the stable scope changes and is not the
  final-tree acceptance result; intermediate compile failures were corrected.

## C3 managed private-history origin binding

The production App installs an SDK private-history policy using the existing
persistent installation key. A managed successful HTTP attempt seals supported
Anthropic signed/redacted thinking and Gemini thought signatures with an opaque
`provider_metadata.bitrouter.nativePrivateOrigin` token. It binds the authenticated
owner, provider, service model, protocol, effective endpoint, account label,
actual successful credential/scope authority, complete assistant message and
part position. Every origin marker is excluded together when committing that
message; other content and metadata remain covered. Supported Gemini text and
media parts now preserve their signatures as well as reasoning and function
calls. The marker is not a provider wire field or a second history store.

The App validates proof, owner and message integrity immediately after
authentication and before other preparation/checker work. The SDK repeats local
validation before request checkers, including context reconstruction. Candidate
preflight checks source identity without credential or provider I/O; final
request validation checks actual authenticated authority on every retry. A
remaining origin marker cannot bypass integrity checks by having its provider
namespace removed. Completely removing both private classification and proof
cannot be detected by this stateless message scheme alone; core's independent
frozen-history checks still prevent preparation hooks from doing that to required
history.

Opaque Anthropic redacted thinking is excluded from readable checker input and
counted separately in `excluded_private_fragments`. Readable reasoning and text
still enter the checker. Managed generation and count clients disable automatic
redirects, and authentication cannot rewrite their expected final URL. Ordinary
compatibility requests keep their existing redirect policy.

A private SDK attempt slot captures the successful HTTP target, final authority
and parsed-result commitment. Count work, failed attempts, custom executor
results and mutated results cannot supply that provenance. Sealing happens before
both the attempt report and returned result are produced. Missing client tool-call
IDs are assigned once before sealing and remain internal correlation identities.
If proof creation fails, output and usage survive, all old/partial markers are
removed, and the receipt explicitly records unverified evidence.

`NativeAttemptReport.private_context` separately describes input and output as
`unknown`, `not_present`, `verified` or `unverified`. Input evidence starts only
at generation dispatch; old receipts default to unknown. These source bindings
do not prove cache reuse, context equivalence or provider acceptance. Core stores
the sealed message and the same receipt. A source-target mismatch can use the
existing one-shot reconstruction path only for explicitly discardable whole old
messages; mandatory history is retained and rejected if no route can serve it.

This segment is limited to supported non-streaming provider-private parts and
existing in-process core execution. Full Responses continuation adjustment,
OAuth principal support, child/reuse/fallback conformance, complete accounting,
C4–C6, A01–A23 acceptance, final audit and PR/CI remain open.

Independent stage review found dropped Gemini signatures on ordinary text/media
and a missing thought flag on signed media; both were fixed and covered by the
production HTTP fixture. Orphan markers still require integrity validation.
Review found no remaining blocker for this segment and confirmed reconstruction
still requires authorized settled messages and versioned required material.

- Focused SDK/production-App/core regressions: 8 passed, followed by the new
  key-unavailable real-output/usage/settlement regression in the workspace run.
- Workspace nextest: 3772 passed, 22 skipped, in 213.866 seconds.
- Final count-URL normalization uses parsed URL equality, matching generation;
  the eight real core input-count integration tests were rerun and passed after
  that last adjustment. The workspace run above preceded that small adjustment.
- Strict all-target/all-feature workspace clippy, all-feature workspace doctests
  (5 passed, 1 ignored), formatting and diff checks passed on the adjusted code.
- The initial broad build lost its output directory before tests could start;
  a missing coverage field in the regex-checker test fixture was also corrected.
  Neither earlier failed check is included as passing evidence.

## C3 Responses reasoning replay prerequisite

The shared non-streaming Responses adapter now retains every reasoning item in
`provider_metadata.openai.reasoningItem`, including its original item ID, ordered
summary parts, empty summary, encrypted content and status. Canonical reasoning
text contains only the readable summary. The request renderer replays the item
without flattening it or moving message text across reasoning/tool boundaries.
Response rendering preserves reasoning items as ordered boundaries; the legacy
conversion within intervening non-reasoning segments is unchanged.

Managed replay requires an assistant role, a nonempty item ID, the supported
reasoning schema and a summary matching canonical text. Both encrypted items and
stored item references use the existing owner/target/actual-authority origin
proof. Raw provider output with unsupported fields remains available in the
result, but cannot silently enter the next managed request. This proves source
identity, not that a provider will retain or accept an item indefinitely.

Readable summaries enter request checkers while encrypted payloads remain
excluded and counted in coverage. Unknown readable metadata, inconsistent text,
and contradictory foreign opaque markers are rejected. Validation also runs on
ordinary HTTP input, before named-router checkers for direct native calls, and
at Responses rendering, so this protection does not depend on managed origin
validation. Independent review found the ordinary-input and mixed-metadata
checker bypasses; both received production App regressions.

Opaque reasoning is ineligible for the existing protocol-neutral visible-history
commitment. Ordinary HTTP continuation still publishes the gateway handle and
substitutes the actual provider response ID for a matching suffix follow-up;
it cannot detach merely because summary text matches. The production fixture
verifies that behavior and the exact tool-result-only upstream input.

SDK fixtures cover encrypted/empty/stored reasoning items, replay order,
malformed fields and summary mismatch. Production App fixtures cover real wire
replay, checker coverage, tampering, owner/model/key/installation changes and
installation restart across the three supported private-history protocols.
The CoreSession fixture now also exercises Responses history and receipts,
rejecting a model change unless complete old messages are explicitly discardable
and required versioned material is available for reconstruction.

This is a prerequisite for native Responses continuation, not its completion.
Native response-handle selection and adjustment receipts, complete output-item
fidelity, stream-bridge private state, child/reuse/fallback conformance, complete
accounting, C4–C6, A01–A23, final audit and PR/CI remain open.

Final independent read-only review found no remaining blocker in this segment.
Validation is separate from that review:

- Workspace nextest: 3776 passed, 22 skipped, in 220.600 seconds.
- A final test-helper-only removal of a redundant `Ok(...?)` wrapper addressed
  strict clippy; all 11 focused SDK/App/core regressions passed after that cleanup.
  The broad run above used the equivalent helper before the cleanup.
- Strict all-target/all-feature workspace clippy, workspace doctests (5 passed,
  1 ignored), formatting and diff checks passed. The initial clippy result failed
  on the redundant wrapper and is not counted as passing evidence.


## C3 native Responses continuation and redacted receipts

Managed non-streaming Responses execution can now retain an encrypted
`provider_metadata.bitrouter.nativeContinuation` artifact on the first part of
its assistant message. It uses the existing installation key with a distinct
AEAD domain; it does not introduce another transcript store. The artifact binds
the authenticated owner, serving target, final credential/scope authority,
requested effective effort (including the absence of an explicit effort), whole
assistant message and exact ordered canonical prefix. Only the built-in
successful HTTP attempt can supply its source, and the actual dispatched prompt
must equal the prepared prompt before a prefix can be certified.

Core plans and durable history keep the complete prompt. The executor alone
renders a suffix and substitutes the provider ID when the binding matches.
Current instructions, tools and settings are still sent. Input counting uses
the same finalized generation view, and final authentication is rechecked for
both operations and retries. The plaintext response ID is removed from managed
Responses output before attempt receipts or core checkpoints receive it,
including unverified custom/stream-bridge output. A private terminal-valid bit
preserves the preexisting terminal checks without treating an unverified result
as proven provenance. Ordinary HTTP gateway continuation remains independent.

Target, effort or prefix changes permit full-history detachment only when the
retained provider output has a supported replay form and private-origin checks
also allow the candidate. Unsupported or lossy output receives a source-proven
`nativeContextRequired` marker with a distinct successful-output nonce. It is
included in message/prefix commitments and requires covering stored state;
deleting the newest handle, selecting an older anchor or merging another branch
cannot silently replay that output. Even identical visible projections cannot
exchange handles for different hidden state. An output with no canonical parts
retains an empty text carrier for this marker. As with the existing stateless
origin proofs, removing all provenance and state markers from arbitrary caller
input is not detectable by that proof alone; core frozen-history validation
separately protects required retained content.

Only a response confirming `store: true`, an outbound request allowing storage,
a matching actual effort and a verified authority can issue a handle. Seal
failure preserves the billed result and usage with an explicit unverified
reason. Unstored lossy output retains its state requirement and cannot be
silently replayed. Planned route decisions and actual attempt observations are
separate: full-history reason, resumed prefix length or rejection on input;
issued, not-stored, not-supported or unverified on output. Unknown legacy fields
remain unknown. Neither a shorter wire suffix nor a stored response establishes
cache hits or token savings.

Independent review found and corrected missing coverage after artifact removal,
lossy mixed-branch detachment, successful-input binding, unverified response-ID
exposure and indistinguishable hidden output states. Production App regressions
exercise suffix/count parity, installation restart, target/effort/prefix changes,
owner/message/token/key checks, state coverage and paid output on seal failure.
A real CoreSession runs three tasks, retains full history, records encrypted
artifacts in checkpoints and dispatches only the proper suffix on follow-ups.
An SDK HTTP regression changes the explicit execution prompt while retaining
its original pipeline context and verifies that no false prefix is certified.

This segment covers the built-in non-streaming HTTP executor. Stream-bridge
private source binding, child/reuse/fallback combined acceptance, complete
output-item fidelity, full monetary accounting, C4 recovery, C5 remote API and
C6 production conformance remain open. A provider can expire or reject stored
state; this local proof does not guarantee indefinite availability. Full A01–A23
acceptance, final audit, PR and CI remain required.

Final independent read-only review found no remaining blocker for this segment.
Validation on the reviewed implementation:

- Focused SDK/App/core selection: 12 passed, including all 10 new regressions.
- Workspace nextest: 3786 passed, 22 skipped, in 217.963 seconds.
- Strict all-target/all-feature workspace clippy passed without warnings.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- Earlier runs exposed a test-only missing dependency, the interaction between
  ID redaction and terminal validation, and two fixture assertions that needed
  to distinguish current instructions from input and decode checkpoint payloads.
  Those failed runs are not counted as passing evidence.


## C3 native continuation with allocation, reuse and fallback

Production App/CoreSession fixtures now exercise native Responses continuation
together with core collaboration and actual HTTP dispatch. Eight child cases
combine replayable or lossy provider output, inherited or fresh allocation, and
the original or a changed model. The parent emits `spawn_agent` during its active
turn. Each child step retains exactly the selected parent step's input plus its
task, or just its task for fresh allocation. The inherited snapshot excludes the
parent's current unpaired tool call and uses the earlier stored anchor for
same-model continuation.
Tests compare complete wire input arrays for suffix, fresh and detached requests.
Replayable inherited history can detach to another model; lossy history rejects
that change before any child HTTP request or attempt.

The worker-reuse fixture directly invokes `CoreSession::collaborate` with
`Action::Delegate`. The selected idle worker resumes its own stored history while
receiving current root instructions. It checks that the harness checkpoint saves
the worker history and continuation receipt; it does not test restoring or
rebinding that checkpoint. A failed stored-state request
also exercises provider fallback: replayable history reaches the backup in full,
whereas lossy history cannot reach it. The failed and successful attempts have
separate receipts, and a subsequent request resumes the backup's response handle,
confirming that the newly issued artifact identifies the actual successful source.

Independent read-only review identified missing positive cross-model detachment
coverage and overly permissive child-history assertions. The final fixtures
include both fixes; follow-up review found no remaining blocker for this segment.
Focused validation passed all three integration tests, including the eight-case
child matrix. Earlier compile and terminal-parent fixture failures are excluded
from passing evidence.

These tests establish combined non-streaming Responses allocation/continuation
behavior. They do not establish stream-bridge private provenance, complete
output-item fidelity, full monetary accounting, C4 recovery, C5 remote API or
C6 production conformance. Full A01–A23 acceptance, final audit, PR and CI remain
required.

Validation on the reviewed test code:

- Workspace nextest: 3789 passed, 22 skipped, in 220.557 seconds.
- Strict all-target/all-feature workspace clippy passed without warnings.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.


## C3 durable cost work inventory

`SessionSnapshot.cost_work` retains work by its original run, independently of
the currently visible root run or worker turn. Preparation, provider attempts,
input counts, context validation, reconstruction and workspace invocations are
projected from their execution records into the same proposed checkpoint. Work
identities preserve agent, turn and step attribution; count identities include
the step so repeated counts after reconstruction cannot overwrite one another.
Retired child turns and prior root runs retain their records. Legacy snapshots
without a run entry have unknown coverage, not a reconstructed zero-cost run.

Intent and outcome states describe durable execution evidence, not billing
status. Model-token estimates retain the original receipt evidence separately
from the explicit unknown total cost. Missing operation duration remains absent,
and observed durations exclude acknowledgement waits. The inventory is not a
second bill and must not be summed with token subtotals or request settlement.
Repeated checkpoints and duplicate tool results do not add work. Conflicting
work identities or changes to recorded outcomes reject the proposed checkpoint.
All retained records count toward the existing checkpoint size bound.

Context reconstruction now commits an intent before local work begins. A failed
outcome commit leaves that intent pending. Rebuilt contexts use their separate
validation/count records and do not pretend to repeat App/router preparation.
Preparation rejected before any provider attempt still retains unknown expense.

Independent review found that the existing validation wall clock includes live
gates awaiting another checkpoint's ACK. A request-local timer now accounts for
all core gate callbacks, including checks inside App transforms. Validation
reports retain the original wall clock and add optional `work_elapsed_ms` after
subtracting those callbacks. The inventory uses only this new measurement.
Uninstrumented custom controls and legacy reports keep it unknown. A real
concurrent checkpoint barrier holds an ACK for 1.2 seconds and verifies that wall
time includes the wait while work time and the run's active budget exclude it.

Focused tests cover outcome ACK visibility/failure, unknown failed fallback,
retired child turns and new runs, preparation rejection, tool result deduplication,
checkpoint serialization, repeated count rounds and reconstruction ACK barriers.
This is the durable work inventory, not complete monetary accounting: reported
and reconciled charges, delayed evidence import, internal authentication retries,
and remaining material/authentication integration costs still need real sources
and correlated coverage. C3 completion, C4–C6, the full A01–A23 audit, PR and CI
remain open.

Final independent read-only review found no remaining blocker after the timing
fix. Validation on the final implementation:

- Accounting/reconstruction selection: 18 passed, including two new integration
  tests and expanded retirement, checkpoint and concurrent-wait assertions.
- Workspace nextest: 3791 passed, 22 skipped, in 234.033 seconds.
- Strict all-target/all-feature workspace clippy passed without warnings.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- An initial check used a nonexistent error constructor; it was corrected before
  executable validation. The first broad pass preceded the timing fix and is not
  used as evidence for that fix.

## C3 retained settlement evidence

The production App installs a read-only `NativeCostSource` backed by its existing
metering store. Lookup uses exact SDK request IDs and the authenticated caller's
user and API key. Missing and foreign records return the same unavailable result.
Reads neither run generation nor recalculate prices, charge the caller or initiate
reconciliation. Hosts without a source retain explicit unknown observations.

`CoreSession::refresh_costs` imports evidence into the original run, including a
retired root run. Each managed model step also attempts a refresh after settlement.
All batches share a five-second read deadline and observe harness disconnect.
Reads hold neither the input nor commit lock; controls can proceed while a source
waits. After reading, the core rechecks operation replay and request ownership
before proposing `cost.observed`. Source errors or timeouts leave existing work
costs unknown and allow model output processing to continue. A failed durable ACK
still blocks progress, as it does for other checkpoint transitions.

`RunCostWork.charges` stores separate estimated, reported and reconciled claims,
keyed by a digest of source, bill ID and basis. A bill belongs to one request and
one run across all bases, including retained earlier runs. Identical evidence is
idempotent; contradictory content or reassignment rejects the complete proposed
transition. `charge_unknown` records the latest read's unresolved request state
without erasing older claims. Neither map replaces the unknown full-work cost.

Configured metering amounts cover model tokens only. The projection excludes
legacy synthetic zero-usage rate-limit/policy rejections: those rows do not prove
observed zero tokens. Stored authoritative receipts provide reported amounts;
matching accepted settlement state additionally provides reconciled amounts.
Explicit accepted no-charge receipts preserve known zero. Estimates, reports,
reconciliation, attempt token evidence and existing request settlement are
different views of a bill and must never be added together.

Production App/CoreSession fixtures exercise fallback sharing one billing ID,
caller isolation, late reconciliation into a replaced run, explicit no-charge,
unknown usage, legacy rejection zeros, duplicate reads and conflicting receipts.
Core fixtures cover monetary ACK visibility/failure, source failures and retry,
cross-run bill identity conflicts, and pending reads during cancel/disconnect.
These tests use loopback providers and SQLite with the real metering recorder and
receipt application path. They do not establish live-provider reconciliation,
full cost coverage, crash recovery or remote API conformance. Internal HTTP
authentication subattempts and auxiliary/material/authentication expenses remain
open C3 work; C4–C6, A01–A23 acceptance, final audit, PR and CI remain required.

Independent review identified the input-lock/read barrier, synthesized zero
estimates, bill reassignment across runs and a follow-up race between source
failure and provisional signal/cancel blocks. Automatic read failures now abort
only on lost authority or commit uncertainty; temporary dispatch blocks still
permit recording already received model output. The deterministic regression
holds a List ACK, polls a signal update into its provisional block, then releases
the failed source and requires the model step to settle before the signal resumes.
Final independent read-only review found no remaining blocker for this segment.

Validation on the reviewed implementation:

- All nine new monetary evidence integration tests passed in 5.724 seconds.
- Workspace nextest: 3800 passed, 22 skipped, in 236.228 seconds.
- Strict all-target/all-feature workspace clippy passed without warnings.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- An initial check exposed a missing tracing dependency, fixed before executable
  validation. An early rejection test mixed 503 and 429 responses, exercising an
  aggregated failure rather than the intended legacy zero normalization. The final
  regression uses two 429 endpoints and verifies that the actual persisted row
  has the synthetic zero while the core retains unknown monetary evidence.

## C3 provider integration work and internal HTTP retries

Managed execution now observes installed authentication body preparation, each
authenticated request build, credential refresh and each HTTP dispatch inside a
selected provider attempt. SDK callbacks carry only request/route/work identity,
phase, duration, received HTTP status and a controlled error category. They do not
carry credentials, URLs, request bodies or upstream diagnostic text. Each core
attempt retains ordered `provider_work` intent/outcome records and projects them
into its original run's cost inventory. Missing legacy or custom-executor records
remain unknown coverage. Parent and phase durations overlap and are not additive.

Every phase waits for its own intent ACK and rechecks current source, permission,
cancellation, active-time and durable-authority gates before running. HTTP status
records describe receipt of response headers, independently of successful body
decoding or generation. A phase's outcome is flushed before the next integration
operation or after executor completion. This permits draining already accepted
response bodies and bridged streams before waiting for an outcome ACK, while
still preventing authentication refresh or retry after a missing ACK. The SDK
continues settlement for received usage when an outcome cannot become durable.

An outer provider attempt reserves the first HTTP dispatch. Each additional HTTP
request after credential refresh reserves another shared model attempt, in the
same checkpoint as its intent. The outer result's token estimate covers only the
terminal result; the extra request adds unknown token expenditure, not a free
401. Unknown admitted work remains pending when cancellation prevents dispatch.
The retry and its original request still share a single settlement/bill identity.
Managed reqwest clients disable implicit protocol retries as well as redirects,
so a client call cannot silently repeat HTTP work outside core admission.

All integration callbacks pause the run's active clock. The SDK separately
subtracts their duration from its attempt report and the executor's upstream
generation time, using each interval's own baseline. End-to-end request latency
remains wall time. Actual continuation/private-context dispatch observations are
recorded after the HTTP intent ACK, immediately before executing the request.

Independent review found and corrected premature continuation observations,
reqwest's implicit retries and upstream generation timing that included ACKs.
The follow-up read-only review found no remaining blocker in this segment.

Validation on the reviewed implementation:

- Focused `test(provider_work)` nextest selection: 7 passed, including six new
  integration tests and the existing input-count cancellation regression.
- Rust 1.95.0 workspace nextest with all features: 3806 passed, 22 skipped, in
  275.724 seconds. This includes ordinary HTTP behavior and the new core fixtures.
- Strict all-target/all-feature workspace clippy passed without warnings.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- Initial test compilation exposed a fixture recorder ownership mismatch and an
  unused import; both were fixed before final validation. The initial focused
  run used the host's updated Rust 1.99.0; final workspace checks used the
  previously recorded Rust 1.95.0 validation toolchain.

The new HTTP fixtures use Chat Completions and production metering, with explicit
ACK loss, cancellation, failed refresh, retry-budget and timing assertions. They
do not establish complete native SSE bridge provenance/continuation, live-provider
behavior, auxiliary/material monetary coverage, or crash recovery. Those C3
requirements and C4–C6, A01–A23 acceptance, final audit, PR and CI remain open.

## C3 native Responses SSE bridge provenance

The built-in native SSE bridge now renders the same authenticated continuation
view as direct execution. The complete canonical prompt remains in plans and
history; only the actual wire request uses a verified suffix. Input counting
commits the actual streaming request shape and endpoint, including the stream
flag, so generation cannot silently diverge from its counted view. Dispatch
observations are shared with direct HTTP and occur after durable admission.

The bridge captures the successful request's final credential/scope authority,
storage permission and effective-effort binding. It consumes the complete SSE
stream through the existing decoder's terminal/EOF checks, then parses the full
terminal response through the shared Responses adapter. Ordered reasoning items,
encrypted metadata, empty summaries and supported provider output no longer
depend on their lossy delta projection. Terminal content replaces that projection
instead of being concatenated with it. Source proofs, encrypted continuation,
raw-handle redaction and state-required markers use the existing native policy.

A missing or non-array terminal `output` cannot authenticate a complete local
message. The bridge preserves received delta content and usage but does not
register a successful source or issue a continuation. Managed output carries
unverified state and cannot silently re-enter a subsequent model request.
Ordinary legacy bridge calls still receive their folded result. Unsupported
items inside a complete output retain the existing state-required semantics;
this does not expand the shared adapter's supported replay vocabulary.

Eight new loopback tests use the production installation-key policy with the real
SDK HTTP executor, including complete reasoning order, unstored replay, stored
suffix/count parity after App reconstruction, actual-key changes, incomplete
usage, invalid terminal/EOF, malformed output, outbound storage denial and two
CoreSession turns. Real TCP fixtures deliver a valid terminal before truncating
the HTTP body or exhausting the request's total timeout. A proven-authentication
retry fixture rejects reuse by the failed principal and permits only the actual
successful principal. Existing HTTP retry fixtures now exercise both direct Chat
Completions and the Responses bridge for budget, cancellation, ACK loss, failed
refresh, timing and single settlement.

These bridge fixtures use static or fixture authentication. The production Codex
OAuth adapter still removes the required output-token limit and remains
ineligible for managed core execution; its hard admission rule is unchanged.
The tests do not establish live subscription support, remote streaming API
conformance, complete provider output fidelity, full monetary coverage or crash
restoration. Remaining C3 requirements, C4–C6, A01–A23 acceptance, final audit,
PR and CI remain open.

Independent review found the malformed-terminal source-binding gap described
above; missing, null and wrong-type output regressions cover the fix. Follow-up
review found no remaining blocker. The two additional transport/identity
regressions were also independently reviewed before final focused validation.

Validation with Rust 1.95.0:

- Workspace all-feature nextest: 3812 passed, 22 skipped, in 250.046 seconds.
  This run predates only the final two test additions and equivalent fixture
  builder extraction; production code did not change afterward.
- Final `test(stream_bridge) or test(provider_work)` selection: 16 passed in
  9.151 seconds, including all eight new tests and both retry protocol paths.
- Final strict all-target/all-feature workspace clippy passed without warnings.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- An initial module move had one invalid relative import, corrected before
  executable validation. An intermediate run mixed a pre-fix SDK build with a
  strengthened malformed-output assertion and failed; the rebuilt focused run
  passed all 14 then-current tests. A test-only boolean simplification resolved
  the initial strict-clippy finding. Failed runs are not passing evidence.

## C3 material fetch cost ownership

Material requests now retain their initiating run, agent and turn in an optional
`origin`. Fetching happens before model-step admission, so its `material_fetch`
cost entry has no step or SDK billing request ID. Existing step-bound entries
retain their serialized string IDs; a missing legacy material origin stays
unknown. The material request ID keys the work record, not a provider bill.

The existing `material.requested` and `material.resolved` checkpoints atomically
record the work intent and outcome. A shared pending fetch has one owner even
when another child or a later root run consumes its result. Late results and
their durable events stay attributed to the original run and agent after
cancellation or turn replacement. Both successful and unavailable resolutions
remain unknown monetary expenditure; neither harness wait time nor ACK latency
is fabricated as operation duration. Rejected stale-version content leaves the
original intent unresolved and cannot enter a model prompt.

Five new integration tests cover result ACK visibility and replay, an ACK lost
after the harness committed the result, cancelled-run reuse, shared child
fetches, and unavailable/stale requests followed by a new version. The existing
request ACK test now checks the absence of visible work before admission. These
tests verify checkpoint evidence and the current in-process session; they do not
establish crash restoration or a harness monetary evidence source. Auxiliary
callback coverage, remaining C3 requirements, C4–C6, full A01–A23 acceptance,
final audit, PR and CI remain open.

Independent read-only review found no P1/P2 blocker in this segment. Validation
with Rust 1.95.0 passed:

- Material-focused nextest: 13 passed, including the five new regressions.
- Workspace all-feature nextest: 3819 passed, 22 skipped, in 286.399 seconds.
- Strict all-target/all-feature workspace clippy passed without warnings.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.

## C3 preparation callback admission

Managed App transforms and pipeline preparation now share the SDK request ID
before the first transform runs. Each transform, pre-resolution hook,
router-preparation hook, pre-request hook, named-router checker, model selector,
router lookup and route hook has an acknowledged intent and outcome. The
pipeline uses its existing callback order and implementations. Ordinary HTTP and
unmanaged native calls have no preparation runtime and retain their existing
behavior. Context reconstruction still uses its frozen validation contract and
does not repeat preparation or model selection.

Core records ordered `preparation_work` on the original model step and projects
each callback into the run's retained cost inventory. Every intent rechecks
source, run limits and turn identity; after its ACK, the live gate checks
cancellation and current permissions again before invoking the callback. An
already running callback can finish after cancellation, but its outcome must be
acknowledged before another callback can start. There are at most 256 callbacks
per step. App and pipeline callback indices have separate sequences; a pipeline
callback cannot be followed by a new App transform. Input counting and final
plan admission verify the same request identity and successful acknowledged
preparation.

Callback reports preserve actual duration and a controlled SDK failure category,
without checker diagnostics, implementation names or input text. Active time
tracks running callbacks and excludes both intent and outcome ACK waits. Work
remains monetary `unknown`; callback completion is neither a zero bill nor proof
of a model invocation. SDK cost lookup now includes request IDs established by
preparation, even when a checker fails before a provider attempt or frozen plan.
Harness material IDs remain excluded. Aggregate preparation and its callback
records are overlapping coverage, not additive monetary charges.

The current deterministic routing path adds no classifier or summarization model
call. This callback observation contract does not authorize hidden model/tool
dispatch inside trusted callbacks or automatically meter such nested work. Any
future model-based preparation must enter managed model admission and consume
the same run budget. Full recovery, remote parity and production-harness
conformance remain required in C4–C6; the complete acceptance audit is still open.

Independent review identified an ACK-time budget race: a different agent could
reserve the final model attempt while a preparation intent waited for its ACK.
The post-ACK preparation gate now checks remaining attempts under the same lock
as source validation and dispatch activation. A deterministic two-agent
regression queues the child's attempt behind the root's held ACK, then verifies
that the root callback does not run. The normal already-reserved model-attempt
path retains its prior dispatch semantics. Follow-up review found no remaining
P1/P2 blocker. Reconstruction coverage also verifies that the rebuilt step has
no second preparation sequence.

Validation with Rust 1.95.0:

- Workspace all-feature nextest: 3825 passed, 22 skipped, in 318.086 seconds.
- Final preparation regressions: all six passed in 5.479 seconds. This final
  run includes an equivalent borrowed-slice test assertion used to satisfy
  strict clippy; production code is unchanged from the workspace run.
- Strict all-target/all-feature workspace clippy passed without warnings.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- Earlier, all 113 then-current core execution tests passed; the subsequent
  ten-test preparation/reconstruction selection also passed after the final
  budget race and request-identity validation changes. An initial obsolete
  single-entry preparation assertion was updated to cover callback exposure;
  its failing run is not passing evidence.

## C3 exit evidence and remaining stage boundaries

This is an implementation-stage exit, not a declaration that A01–A23 or the
requested end-to-end system is complete. Independent scope review checked the
specification against the current routing, allocation, reconstruction and cost
implementation. Optional classifiers/learning, all provider output types and
full beta compatibility are not added prerequisites for C3. Unknown monetary
evidence remains an explicit supported state, not an exact spending cap.

| Requirement within C3 | Current implementation and executable evidence |
| --- | --- |
| Continue, reuse and fresh allocation with explicit overrides (A05/A07) | `core/allocation.rs`; `core_execution` tests for stable idle delegation, ambiguous fresh allocation, explicit spawn, follow-up, workspace provenance and activation after changed signals |
| Immutable required context and feasible prepared input (A07/A08) | `core/routing.rs`, `core/signals.rs`, `core/reconstruction.rs`; capacity, input-count, required-material, stale-version and reconstruction regressions, including frozen revalidation and no repeated selection |
| Decision → application → actual outcome identity (A09) | `ModelStep` decisions/applications/attempt receipts and allocation joins; fallback, worker reuse and reconstructed-plan assertions in `core_execution` and `orchestrator_core` |
| Declared provider constraints and continuation support (A09/A22) | Production App/SDK loopback fixtures in `native_http/protocol_matrix.rs` and `native_http/private_context/`; four-by-four ordinary protocol comparison, authenticated private state, suffix/count parity, actual fallback and complete SSE bridge terminal/EOF evidence |
| Cost provenance and auxiliary work ownership (A21) | `core_execution/accounting.rs`, `provider_work.rs`, `material_work.rs`, `preparation_work.rs` and `native_http/costs.rs`; retained unknown exposure, estimate/reported/reconciled separation, ACK barriers, internal retries, failed preparation and late original-run settlement |

The table identifies supported in-process behavior and current local fixtures.
It does not turn ordinary HTTP protocol comparison into managed remote parity,
serialized snapshots into restored sessions, or loopback providers into live
provider/harness acceptance. Those distinctions remain part of final acceptance.

The next stage is C4: validate and restore committed snapshots/journal heads,
reconcile uncertain provider and tool work with epoch fencing, preserve root
queues/steering/cancellation, and exercise the crash/ACK-loss matrix. C5 adds the
authenticated harness channel and managed Responses mapping over those same
operations. C6 must supply the production harness, independent client, real-task
and pressure evidence. Final A01–A23 audit, PR and CI remain required.


## C4 authenticated snapshot restoration

`CoreSession::restore` reconstructs a session from an authenticated harness's
committed checkpoint and optional contiguous journal tail. It verifies exact
batch bytes/digests, session identity, ownership progression, revisions/sequences,
and agreement with the supplied durable head. A different core instance requires
a higher epoch. The complete restore envelope is bounded by
`unacknowledged_bytes`; the harness can select a recent checkpoint instead of
sending an unbounded history of full snapshots. Snapshot structure and all
required artifact declarations/availability are checked before a new checkpoint
can be proposed. The harness remains responsible for verifying durable artifact
bytes and authenticating the binding.

Creating a replacement scheduler requires `previous_owner_stopped=true`, even
when preserving the same core instance and epoch. This is the authenticated
harness's attestation that the previous scheduler and provider I/O have been
stopped/reconciled, not a conclusion inferred from elapsed time or an epoch
number. The replacement records `session.restored` and waits for its exact ACK
before returning a schedulable session. It extends the recovered head rather than
re-appending historical batches, including an input batch whose original ACK was
lost. A retained complete model receipt is applied through ordinary output
validation without another provider call or SDK settlement. An incomplete step
is marked interrupted and closed; a subsequent call gets a new step/attempt ID,
while original attempt counts, unknown costs, run limits and recorded active time
remain. This does not reconstruct unobserved active duration during a crash.

Pending tool reconciliation has explicit behavior:

| Harness evidence | Restored behavior |
| --- | --- |
| Committed or supplied definite result | Retain/consume the result; never execute the tool again |
| `not_started` | Preserve invocation/attempt identity and authorize delivery under the new epoch after the restore ACK, subject to current permissions, manifest and workspace |
| `running` or `waiting_approval` | Retain the original execution/approval wait; accept its eventual result without sending a second execute |
| Missing, `stopped` without a result, or `effect_unknown` | Preserve uncertainty and block dependent scheduling; a stopped process does not prove absence of effects |

A later explicit restore can reconcile an `EffectUnknown` result with a definite
outcome. The original is retained in `prior_uncertain_result`, including artifact
dependencies; known results remain immutable. Fresh observations are required for
unresolved tools on every restore. Stored `recovery_observation` is audit evidence,
not permission to assume the tool remained unstarted across another crash.
Cancellation messages use the current grant epoch while retaining the original
invocation/attempt identity. The harness must fence old approval authority and
revalidate any permission to execute under the current grant.

Independent review identified and the implementation addresses three recovery
boundaries: workspace-only manifest changes revoke unstarted tool authorizations;
per-turn cancellation intent survives temporary `RecoveryRequired` across multiple
restores; and uncertain tool outcomes can be reconciled without erasing their
original evidence. Newly admitted invocations freeze workspace revision alongside
permission, manifest and signal provenance. Restoration additionally denies known
unstarted calls when the binding's workspace revision changes, including older
snapshots without the new invocation field. Agent cancellation is separately
retained in `cancellation_requested` at all cancellation entry points.

This is the replacement-process path. Live same-owner transport/head reconnect
and late provider evidence are covered by the following increment. Durable root
queue/steering/release, the remaining crash/concurrency matrix, and C5/C6 remain
open. No remote endpoint is introduced
by this change; full A01–A23 acceptance, final independent audit, PR and CI are
still required.

Validation with Rust 1.95.0:

- Twelve new restore integration tests exercise recorded crash boundaries,
  lost input ACK and the restore ACK barrier, complete-output adoption,
  uncertain attempts/costs, definite tool results, approval/running waits,
  workspace changes, two-stage subtree cancellation, uncertain-result
  reconciliation, artifact dependencies, ownership/chain rejection and exhausted
  budgets. One active-time unit test verifies resumed accumulation.
- Workspace all-feature nextest: 3838 passed, 22 skipped, in 300.031 seconds.
  This final run includes all thirteen new tests and the existing core/SDK/API
  regressions. An earlier fixture had mismatched signal/manifest workspace
  revisions and was corrected before this run; that failing run is not evidence
  for workspace-change recovery.
- Strict all-target/all-feature workspace clippy passed with `-D warnings`.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- Independent review's three findings were fixed and re-reviewed with no
  remaining P1/P2 finding for this restoration increment. Full C4 and the final
  requirements audit remain open.

## C4 same-owner reconnect and late provider evidence

`CoreSession::reconnect(grant, durable_head)` reconciles the existing process
after its authenticated host reconnects the harness port. The exact ownership
grant must remain unchanged; another owner or process uses `restore`. The old
driver and every retained SDK execution control must finish before reconnect can
succeed. `Busy` includes detached SDK finalization after the driver is dropped.
Controlled provider execution now observes its original disconnection token;
cancellation still flows through attempt reporting and SDK settlement and never
proves that an upstream operation was free or had no effect.

The harness head must match the acknowledged local head or the exact retained
pending batch. Reconnect adopts an already durable batch or resubmits its original
identity and bytes. No replacement input or provider attempt is generated to
resolve ACK loss. A complete committed model result can pass through ordinary
output admission without a second provider call or settlement. Incomplete steps
close as interrupted before any late evidence is imported. The next attempt has
new step/attempt identities while existing budgets and cost exposure remain.

`session.reconnected` persists the maximum of recorded and still-retained active
time before reconstructing the activity timer. This includes preparation/count/
validation work that finished after disconnect without an outcome checkpoint.
Adopting a pending new root input starts its own timer. Material requests and tool
cancellation may repeat with their original identities. An uncertain tool execute
delivery without a definite result returns `RecoveryRequired` and needs explicit
authenticated restoration/tool reconciliation; it is never blindly repeated.
Detached delivery errors can close only their originating connection generation.

`pending_provider_evidence()` exports a bounded, unacknowledged buffer for an
authenticated harness to preserve before replacement. `provider_evidence(op_id,
ProviderAttemptEvidence)` imports the corresponding `model.evidence` operation.
Evidence contains the original run and attempt IDs, a complete `NativeAttemptReport`,
and optional measured cumulative active time for the original run. The retained
cost inventory freezes route/index/request provenance and exact outcome digests,
including after the original turn retires. Imports reject foreign or conflicting
reports and retain original cost ownership without charging a newer run. They
never apply assistant output, execute tool calls, or repeat SDK settlement.

Volatile reports and the pending checkpoint envelope share the unacknowledged
byte bound. Overflow is explicit and blocks live reconnect until the harness
performs replacement reconciliation; unknown costs are not converted to zero.
Legacy archived attempts without frozen admission cannot authorize a new report.
Evidence is visible as committed state only after its matching durable ACK.

Independent review found and verified repairs for active-time loss, stranded
delivery markers and stale detached sends. The reconnect regressions cover exact
ACK retransmission/adoption, committed-output reuse, bounded late output without
tool execution, buffer overflow, old-run evidence after root replacement,
disconnected preparation time, abandoned drivers, SDK settlement, material retry,
uncertain tool delivery and cancellation retry. Remote endpoints, root queue/
steering/release, production harness conformance and the final A01–A23 audit remain
pending.

Validation with Rust 1.95.0:

- Eleven new integration tests plus two existing reconnect/ACK regressions:
  13 passed in the focused selection. The cancellation test covers both a live
  driver and a previously abandoned driver with detached SDK finalization.
- Workspace all-feature nextest: 3849 passed, 22 skipped, in 306.369 seconds.
- Strict all-target/all-feature workspace clippy passed with `-D warnings`.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- Independent follow-up review confirmed all three findings were fixed and
  found no remaining P1/P2 issue in this increment.

## C4 durable root queue

`CoreSession::enqueue(operation_id, expected_revision, input)` durably reserves
the original operation, run and agent-turn identities in `input.enqueued`.
Acceptance does not change active history, consume a model attempt, or start a
second driver. Pending root inputs use the session binding's `queued_runs` cap;
each input retains its own frozen execution limits. Direct `start` cannot bypass
pending FIFO work. Duplicate operations retain their original acceptance receipt
even after activation or cancellation; conflicting payloads are rejected.

At a settled root boundary the existing driver takes the next eligible item and
commits `input.accepted` with its reserved identities. Only that ACK exposes the
new root context, resets its active-time budget and allows model preparation.
The driver can continue through multiple queued runs until work is blocked,
paused or exhausted. Terminal success is committed before the next activation;
owned child work, dispatched tools and retained SDK finalization must settle.
Run cost inventory remains attributed to its original identity across advances.

Root failure, cancellation and recovery-required states persist a queue pause.
`resume_queue(operation_id, expected_revision)` clears it only after execution
and effects settle; it cannot bypass uncertain tools. Resume has its own ACK and
does not create a driver. `cancel_run` also accepts a reserved queued run ID:
`input.cancelled` removes that input, preserves the current run and tools, and
pauses subsequent advancement. Cancellation and activation serialize on the same
input gate, including cancellation waiting behind another checkpoint. An input
that already became active follows ordinary active-run cancellation semantics.

Queued inputs retain required material IDs from acceptance and later signals.
Current material availability and verification permissions are checked again at
activation; infeasibility preserves the head input and commits `queue.paused`
instead of silently dropping it or skipping to a later task. Restoring the
missing requirements and explicitly resuming retains the same input identity.

Independent review found that appending harness requirements directly to a
near-limit user input could create a committed snapshot that restore rejected
under the ingress byte bound. The queue now retains the bounded original
`input` separately from cumulative `required_materials`; the complete snapshot
still obeys its checkpoint byte limit. Restore validates those distinct fields,
reserved IDs, original receipts, queue capacity and frozen execution limits.
Operation, run and turn identity namespaces remain separate. Follow-up review
confirmed this repair with no remaining P1/P2 finding in the queue increment.

This implements root queue control in process, not steering, session release,
remote Responses/channel integration or complete C4–C6 acceptance. Those remain
required, together with the final A01–A23 audit, PR and CI.

Validation with Rust 1.95.0:

- Thirteen new root-queue integration tests passed. They cover FIFO context and
  identity, tool cleanup, persistent failure/cancellation pauses, queued cancel,
  acceptance/activation/resume ACK loss, reconnect and replacement restoration,
  input/count bounds, required-material changes, cancellation during a held
  signal ACK and semantic conflicts in correctly encoded checkpoints.
- Restore verifies the original enqueue fingerprint and explicit frozen limits
  before appending any new batch. The near-limit signal regression verifies that
  a legally accepted queued input remains restorable and cancellable.
- Final workspace all-feature nextest: 3862 passed, 22 skipped, in 312.601 seconds.
- Strict all-target/all-feature workspace clippy passed with `-D warnings`.
- Workspace doctests: 5 passed, 1 ignored; formatting and diff checks passed.
- The final independent review found no remaining P1/P2 issue in this queue
  increment. Full-stage and end-to-end acceptance remain open.

## Rust 1.99 CI compatibility

The first draft PR run used Rust 1.99 and exposed failures not covered by the
earlier Rust 1.95 checks. Stable CI retains its strict warning policy; the
workspace's minimum Rust version remains 1.93.

- Upgrade only `async-trait` from 0.1.89 to 0.1.92. Its
  [upstream fix](https://github.com/dtolnay/async-trait/releases/tag/0.1.92)
  removes a redundant generated `must_use` attribute that caused 76 SDK clippy
  errors. The updated macro uses the already locked `syn` 3.0.3; both dependencies
  declare Rust 1.71 as their minimum version.
- Replace deprecated `fetch_update` calls with equivalent weak compare/exchange
  loops in preparation indexing, provider and core gate timing, and host reload
  environment revisions. The loops preserve the old success/failure orderings,
  saturating counters and rejection of exhausted preparation indices, without
  depending on a newly stabilized atomic API.
- Format four protocol references as rustdoc links. No protocol content changes.
- An independent review of the complete source and dependency diff found no
  actionable P1/P2 issue.

Validation of this compatibility increment is recorded separately from C4's
remaining steering, release and live tool-status work. It does not establish
remote API or production-harness acceptance.

- Rust 1.99 `cargo clippy --workspace --all-features --all-targets -- -D warnings`
  passed.
- Rust 1.99 `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features
  --no-deps` passed; workspace doctests passed (5 passed, 1 ignored).
- Rust 1.93 `cargo check --workspace --all-features` passed.
- Rust 1.99 SDK checks with `RUSTFLAGS='-D warnings'` passed both without default
  features and with only `config_file` enabled.
- Rust 1.99 workspace all-feature nextest passed: 3862 passed, 22 skipped, in
  305.174 seconds, using one test thread.
- Formatting and diff checks passed.

The subsequent CI run passed compilation and macOS tests, but its Ubuntu
workspace test job exceeded the five-second final completion watchdog in
`implicit_assignment_cycles_reject_before_acceptance`. That check executes five
agent turns with durable checkpoints; its cycle-rejection and unchanged-head
assertions had already passed. The watchdog is now 30 seconds, with all graph
and completion assertions retained. Independent review found no removed timing
requirement or correctness coverage. The targeted regression passed, then Rust
1.99 workspace all-feature nextest with four test threads passed: 3862 passed,
22 skipped, in 79.701 seconds. Strict clippy, formatting and diff checks passed.

The pushed `9907c486` CI run subsequently passed every executed check, including
Linux/macOS/Windows tests and clippy, MSRV, docs and feature isolation. Publishing
jobs were skipped by workflow policy.

## C4 targeted steering and tool start fences

`CoreSession::steer` accepts a bounded text input for an exact active run/agent
turn. `input.steer.received` and `input.steer.applied` are separate acknowledged
transitions; the original acceptance receipt remains immutable. Pending input
is ordered by receipt revision, retained through reconnect/replacement, scoped
to its target, and marked cancelled with cancellation of the owning turn/run.

Receipt leaves the running step and prompt immutable. Targeted provisional
admission fences wait for the input serializer without failing unrelated agents;
rejected or abandoned requests release those fences. Durable receipt prevents
new target preparation, input counting, provider attempts/fallbacks, workspace
dispatch and collaboration. SDK execution/settlement retains original evidence
and costs; superseded output cannot start its dependent calls. Application waits
for SDK quiescence and definite tool outcomes, consumes paired call/results,
then appends input to history/required instructions and advances context revision.

`CheckpointPayload.tool_start_fences` makes the harness half of that admission
barrier explicit. Each pair identifies an outstanding target invocation/attempt
within the batch's session. The harness must persist tombstones atomically with
the checkpoint/head, serialized against actual tool-start admission after
approval and policy checks. This also covers commands not yet delivered locally.
An older peer that cannot enforce the field must reject, including initial bind;
new proposals always carry it. Legacy persisted payloads without it remain
decodable. Tombstones survive replay, restart, epoch change and compaction.
Fences do not stop running tools or establish an outcome: the harness returns
durable `not_executed` for prevented starts and actual results for existing work.
Core continues to wait for those results, including restored approval waits.

The deterministic durable fixture now models this atomic start/fence ordering.
It is not a production harness implementation. C4 live tool-status observations,
session release and the remaining fault matrix, C5 transport/auth integration,
C6 production conformance and the full A01–A23 audit remain open.

Independent review identified and drove fixes for unrelated-agent admission
failure, scheduler starvation during SDK finalization, pending approvals retaining
start authority, and detached live SDK execution exceeding model concurrency.
The scheduler counts outstanding execution across driver replacement while
allowing independent work once only finalization remains. The accounting
regression's control future now continues after its held List ACK: output shares
the input serializer, so a deliberately suspended queued control cannot be held
until output completion. Its failure-during-provisional-block and preserved-output
assertions remain in place.

Validation of the final source on Rust 1.99:

- Workspace all-feature nextest: 3879 passed, 22 skipped, in 91.084 seconds,
  using four test threads. This includes fifteen new steering integrations and
  two checkpoint start-fence tests, plus the existing accounting regression.
- The targeted eighteen-test run passed. Coverage includes separate receipt/
  application ACKs, two directions of ACK loss, exact replay and forged restore
  rejection, ordered input restoration, cancellation, child targeting/bounds,
  held provider output, first/fallback admission, unsent tool/collaboration
  supersession, approval/start ordering, lost-ACK/restart/epoch fences,
  unrelated-agent admission and detached execution versus finalization capacity.
- Strict workspace/all-target/all-feature clippy with `-D warnings`, strict
  workspace rustdoc with `RUSTDOCFLAGS=-D warnings`, formatting and diff checks
  passed. Workspace doctests: 5 passed, 1 ignored.
- Rust 1.93.0 workspace/all-feature `cargo check` passed.
- The final independent review confirmed all four findings were fixed and
  reported no remaining P1/P2 issue in this increment.

Remote CI and complete cross-stage acceptance remain separate gates.

The pushed `c2db646c` steering increment subsequently passed every executed CI
check, including Linux/macOS/Windows tests and clippy, MSRV, docs and feature
isolation. Publishing jobs were skipped by workflow policy.

## C4 live tool observations

`CoreSession::tool_status(operation_id, observation)` records authenticated
harness evidence for an already dispatched invocation/attempt behind its own
checkpoint ACK. Observations and their evidence remain immutable under operation
identity, bounded by ingress and complete checkpoint limits. Exact operation
replay returns the original receipt; a new operation is validated against current
execution facts. Restore checks observation identity, receipt fingerprint,
assigned IDs, accepted disposition and committed revision, and requires every
referenced artifact, including evidence retained from prior restorations.

`stopped` supplies neither an execution outcome nor redispatch permission.
`effect_unknown` blocks the turn/run, pauses the root queue and invalidates
workspace provenance. A later live definite result is retained, but explicit
authenticated restoration must reconcile the blocker. If restoration confirms
the invocation is still running, a fresh uncertainty operation, including one
with new evidence, blocks again. A definite result rejects a new contradictory
uncertainty claim while preserving exact replay of an earlier receipt.

Ordinary lifecycle observations cannot regress. An authenticated restoration can
prove that a pending approval never crossed durable start admission and return
it to `not_started`; the old owner/approval must be fenced. The new epoch still
passes normal workspace, permission and steering checks before dispatch. Restore
revision distinguishes the superseded approval from later live observations.
Any historical `running` or `stopped` evidence, including evidence from earlier
restorations, prevents this reset. A late status for a retained child invocation
keeps the child's original run attribution after the root starts another run.

Known running tools share the model activity clock: overlapping executions count
once, approval waits add no time, and stopped/definite outcomes end tool activity.
An uncertain effect does not erase earlier running evidence. Same-owner reconnect
keeps local elapsed time accrued while awaiting its checkpoint ACK. Replacement
restoration preserves persisted elapsed time and starts observation of tools
confirmed or previously known to be running without evidence of termination.

This increment does not establish full active-time acceptance. Process downtime
still needs authoritative elapsed-interval reconciliation, and all workspace
dispatch/verification admission paths must enforce the exhausted run budget.
These remain required C4/A20 work, along with session release and the remaining
fault matrix. C5 remote/auth integration, C6 production conformance and the full
A01–A23 audit remain open.

Independent review drove repairs for lost reconnect-ACK activity, renewed
uncertainty being mistaken for an earlier observation, pending approvals that
could not be reconciled as unstarted, irreversible restore evidence being
overwritten, old child observations receiving a new root's run attribution, and
restored uncertainty remaining blocked despite explicit definite-result
reconciliation. The final read-only review found no remaining P1/P2 issue in
this increment; complete C4 and cross-stage acceptance remain separate gates.

Validation with Rust 1.99:

- Twelve focused live-status integrations passed. They cover ACK barriers and
  both forms of ACK loss, exact-operation replay, conflicting IDs/evidence,
  bounded input, artifact and receipt tampering, renewed uncertainty, definite
  result reconciliation, approval restoration, retained execution facts across
  three replacements, old child attribution, overlapping tool intervals and
  held reconnect-ACK timing.
- Workspace all-feature nextest: 3891 passed, 22 skipped, in 87.486 seconds,
  using four test threads.
- Strict workspace/all-target/all-feature clippy passed after collapsing one
  nested condition without changing its behavior. Strict workspace rustdoc,
  formatting and diff checks passed; workspace doctests: 5 passed, 1 ignored.
- Rust 1.93.0 workspace/all-feature `cargo check` passed.

The new increment's remote CI remains separate from these local checks.


The pushed `79228b27` live-status increment passed every executed CI check,
including Linux/macOS/Windows checks, MSRV, docs and feature isolation. Publishing
jobs were skipped by workflow policy.

## C4 active-time admission and cleanup

The session now enforces the latest shared active-time counter before model,
preparation/count/validation/reconstruction, workspace-tool, verification,
material-request and new collaboration work. Admission uses the live union
clock and refreshes the checkpoint counter before validating an intent. It also
checks after an intent ACK, so a held commit cannot authorize execution after
another agent or tool consumes the remaining budget. Pure approval and idle
checkpoint waits remain excluded.

An independent timer observes active work even after `drive()` returns to await
harness tools. It retains only a weak session reference while sleeping and uses
its own notification channel. Exhaustion after the last activity ends still
counts, including restored exhausted counters and the final success boundary.
A timer cannot transfer a previous root's elapsed time or failure to a new run.

`run.limit_reached` atomically commits `RootRun.resource_error` with a typed,
committed `limit_exceeded`, pauses the root queue, removes pending descendant
follow-ups, marks current nonterminal turns for cancellation and fences every
unresolved current-run tool start. The fence has the same harness transaction
requirement as steering: an old pending approval cannot start after that batch
commits. Unstarted local dispatches receive empty `not_executed` results, which
also fit a harness advertising a one-byte output bound. Delivered commands use
the existing cancel/result reconciliation path; a cancel request is not a tool
outcome. Late status, definite results, cost reports and provider settlement
remain admissible.

Cleanup preserves paired history and waits for owned SDK execution/finalization.
If a driver is abandoned, a still-live SDK control prevents premature recovery;
after that control exits, an unapplied model step requires normal reconciliation.
Unknown tool effects remain `recovery_required` with the resource failure and
cancellation intent retained. After all effects settle, the run ends `failed`
with its resource error, including when a later explicit cancel was accepted.
Only explicit queue resume can activate the next root, with a fresh counter.

The driver revisits agents when state changes during asynchronous tool delivery.
It retries a limit rejection only when the same run gained a new committed
resource failure; a cleanup event advancing revision is not sufficient. A cleanup checkpoint that itself exceeds a bound returns its
typed error without a busy loop, retaining its required result and cleanup
state. Reserving sufficient capacity for all cleanup/terminal records remains
separate bounded-operation work; this increment does not claim that guarantee.

Process-downtime interval reconciliation, session release, the remaining C4
fault matrix and capacity work, C5 remote/auth integration, C6 production
conformance and full A01–A23 acceptance remain open.


Independent review identified two further boundary defects. Cleanup errors could
loop after a no-op cleanup event advanced the revision, and late elapsed-time
evidence could overtake the preliminary check before the terminal commit. The
regression for a near-capacity snapshot reproduced the first failure. Both are
fixed: cleanup limit errors propagate, and the terminal closure rechecks the
fresh counter under the commit lock. Final independent read-only review found
no remaining P1/P2 issue in this increment.


Validation of the final source with Rust 1.99:

- Ten focused budget integrations passed, covering pending approvals, held
  dispatch and checkpoint ACKs, both ACK-loss outcomes, detached SDK settlement,
  exhausted restoration before model/verification/terminal admission, unknown
  effects, queued roots, invalid resource snapshots and bounded cleanup errors.
- Workspace all-feature nextest: 3901 passed, 22 skipped, in 88.275 seconds,
  using four test threads.
- Strict workspace/all-target/all-feature clippy and strict workspace rustdoc
  passed. Workspace doctests: 5 passed, 1 ignored.
- Rust 1.93.0 workspace/all-feature `cargo check`, formatting and diff checks
  passed.

The new increment's remote CI remains separate from these local checks.


## C4 settled ownership release

`CoreSession::release(operation_id, expected_revision)` closes the current
ownership grant behind a `session.released` checkpoint. It requires a settled
root, terminal agent turns, paired tool/collaboration results, settled model
steps and no remaining live SDK controls or volatile provider evidence. It does
not cancel an active run or infer execution termination from a dropped driver.
Already durable unknown monetary cost remains retained and does not itself
prevent release. Pending root inputs preserve their accepted identities and
become paused for the next owner.

The immutable receipt identifies the released session, instance and epoch.
Release records remain in the snapshot under operation identity across later
grants. Ordinary ACK adoption and both reconnect reconciliation paths fence the
old dispatch gate in the same live-state critical section. The old transport
cancellation token is closed; a reconnect that adopts release returns that head
without appending `session.reconnected` or renewing execution authority. Reads
and exact operation replay remain available, while new mutations cannot append
under the released grant.

Replacement restoration requires an authenticated higher epoch, even if the
instance identifier is reused; normal previous-owner quiescence requirements
still apply. Restoring or replaying an old release receipt under a higher epoch
does not release the new grant. The retained root queue remains paused until
explicit resume. Restore checks record/receipt fingerprints, identities,
dispositions, revisions and grant ordering, and checks every supplied journal
batch for release after which the same epoch cannot append. Later snapshots
cannot erase or rewrite a retained release record.

Independent review identified a stale delivery marker that could prevent
release after a definite tool result, disconnect, reconnect and successful run
completion. The corrected guard relies on confirmed effect and completed pairing;
an unacknowledged transport completion is not evidence that the tool is still
running. A regression covers this path and confirms no redispatch. Final
independent read-only review found no remaining P1/P2 in this increment.

Session release is now available in process. C5 still supplies its authenticated
wire operation and ownership host integration. Process-downtime accounting,
cleanup/terminal capacity reservation, the remaining C4 fault matrix, C6
production conformance and the complete A01–A23 audit remain required.


Validation with Rust 1.99:

- Nine focused release integrations passed in 0.209 seconds. They cover ACK
  barriers, both ACK-loss outcomes, abandoned release futures, exact replay,
  queued-root handoff across three epochs, record/journal tampering, detached SDK
  finalization and confirmed outcomes after delivery acknowledgement loss.
- Workspace all-feature nextest: 3910 passed, 22 skipped, in 88.431 seconds,
  using four test threads. The existing external-editor PTY test also passed.
- Strict workspace/all-target/all-feature clippy passed after replacing a map
  lookup with the equivalent `contains_key` predicate. Strict workspace rustdoc
  passed; workspace doctests: 5 passed, 1 ignored.
- Rust 1.93.0 workspace/all-feature `cargo check`, formatting and diff checks
  passed. The final independent review confirmed the delivery-marker repair and
  found no remaining P1/P2 issue in this increment.

Remote CI remains a separate gate. The preceding budget commit's macOS job
reported an external-editor PTY submission timeout; its failed job was rerun.
Neither that pending rerun nor the new increment's CI is counted as a pass here.

Subsequent remote verification confirmed the budget commit's second CI attempt
completed successfully. The ownership-release commit `789ce9aa` also completed
its CI workflow successfully across Linux, macOS and Windows.

## C4 cumulative activity handoff on process restoration

`Restore.active_time` accepts a `RunActivityReconciliation` containing the exact
`run_id`, source `durable_head` and cumulative `active_ms`. Every nonterminal
run requires this authenticated evidence before the replacement can commit or
dispatch. A missing measurement returns `recovery_required`; a different run,
stale head or regressing counter is rejected. Settled terminal runs may omit
the measurement, and supplied evidence cannot change their final counter.

The harness/host must attest the union of model, preparation and tool intervals
through entry to `CoreSession::restore`. Concurrent intervals count once; pure
approval or idle commit waits contribute nothing. This contract includes the
uncheckpointed tail before a crash and the measured handoff delivery interval.
An old timestamp, per-attempt sum, or entire disconnect duration is insufficient.
If the host cannot establish this boundary, it must leave restoration blocked.
Core does not infer clock synchronization between separate machines. C5 must
establish the remote measured handoff or quiescent boundary before invoking this
in-process entry; C6 must verify production harness measurement.

The supplied value replaces the cumulative baseline, rather than being added
to the last checkpoint. `RootRun.activity_reconciliations` retains each accepted
measurement. The `session.restored` event contains the same evidence, and every
supplied journal batch is checked for retained prefixes, monotonic counters,
head identity and event/snapshot agreement. ACK loss is reconciled through the
durable head and a fresh handoff; reusing a stale source head does not apply the
measurement again. A new root run starts an independent counter and inventory.

Still-running tools enter the local union clock at restoration entry, including
snapshot validation and the restoration ACK wait. Pure approval remains idle.
`HarnessPort::observe_restoration` installs a weak `RestorationActivity` observer
before the first restore proposal. Running-tool restoration is rejected by the
default implementation. Supporting hosts report actual stop times concurrently
with commit waits; earlier stops can be delivered out of order without moving
the union's end backwards. Pending approvals cannot start during restoration.
Stop observations close the local interval immediately, remain bounded by the
known running invocations, and commit as ordinary `tool.status` receipts before
admission and its budget watchdog open. They do not synthesize tool results.
After the observer closes, the host uses the live session's status/result API.
The observer remains open through all restored model-output checkpoints.
Restoration checkpoints retain the authenticated entry counter while the local
open interval is provisional: a delayed stop can correct that interval without
leaving an inflated durable `active_ms`. Before closure, the host's
`synchronize_restoration` barrier drains earlier lifecycle observations; their
status checkpoints are acknowledged before another drain. No checkpoint wait
follows the final empty drain. Normal admission consults the corrected live
clock immediately, and the next normal checkpoint captures its cumulative value.
Exhausted evidence reaches the existing admission and cleanup path before any
replacement model, tool or verification work can start. Missing activity
evidence remains distinct from an uncertain tool effect and unknown monetary
cost; none can be silently synthesized from the others.

Independent review identified idle time incorrectly charged when a tool stopped
during the restoration ACK wait, before the caller could access the new session.
The observer bridge repairs this blind interval; a regression reports two stops
out of order near the budget boundary and holds the restore ACK for a further
1.1 seconds. It confirms no false limit failure, durable stop facts, exact
duplicate handling and subsequent restoration without duplicate charges.
Follow-up review found delayed stops could arrive after an overestimated counter
was already proposed, and the observer initially closed before recovered model
output replay. Freezing the provisional counter, draining the lifecycle bridge,
and moving closure to the final return boundary repair those cases. Additional
regressions hold the first stop's checkpoint while delivering a delayed second
stop, and stop a root tool during a child's restored output checkpoint.
Failure after the restore ACK now preserves the whole operation's durable
status: later replay/stop proposal rejection or a failed lifecycle drain returns
`committed`, while an unconfirmed submitted batch remains `unknown`. A regression
verifies drain failure retains the restored head and starts no execution. The
final independent read-only review found no remaining P1/P2 in this increment.

Initial focused validation: all 203 `core_execution` integrations passed in
29.757 seconds; the initial full workspace passed 3917 tests with 22 skipped in
89.129 seconds. Those runs preceded the review repair. Nine final focused
handoff tests passed in 1.579 seconds. Final workspace validation and independent
review are recorded below when complete. Remaining
C4 capacity/fault work, C5/C6 and the complete A01–A23 audit remain open.

Final validation with Rust 1.99.0:

- Workspace all-feature nextest: 3922 passed, 22 skipped, in 100.214 seconds,
  with four test threads. This includes all twelve activity-handoff integrations.
- Strict workspace/all-target/all-feature clippy and strict workspace rustdoc
  (`-D warnings`) passed. Workspace doctests: 5 passed, 1 ignored.
- Rust 1.93.0 workspace/all-feature check, formatting and diff checks passed.
- Earlier intermediate validation caught a collapsible-if lint and a test
  calling a crate-private helper; both were corrected. Those interrupted runs
  are not included in the final passing evidence.

The next commit's remote CI is a separate verification gate. The remaining C4
capacity/fault work and C5/C6 production integration remain required.

## C4 bounded tool outcome and lifecycle payloads

New workspace and verification intents freeze `ToolExecute.result_limits`:
`output_bytes` limits UTF-8 output and `payload_bytes` limits the complete
serialized result/status object, including JSON escapes, evidence reference
metadata and workspace revision. The latter deducts the largest control
message envelope from the caller-lowered run input bound. A result at its
payload bound fits even maximum-length session/operation identities, epoch and
expected revision. Local calls and restore reconciliation use the same bound;
remote ingress additionally validates the full wire message.

Live admission checks the host bound before fingerprint serialization and the
frozen invocation bound before proposing a checkpoint. Rejection leaves the
head and operation identity available for retry with valid evidence. It does
not truncate or synthesize a successful result. Signal updates and later root
runs cannot enlarge an admitted reply. Counting uses a writer rather than
allocating another serialized copy solely to measure its length.

Restoration validates retained results, uncertain prior results and lifecycle
observations, then validates newly supplied evidence before any proposal or
execution. Legacy intents without `result_limits` derive their bound from the
matching retained run or the original turn's explicit inherited input limits.
The restore checkpoint freezes that migration before redispatch. If neither
source remains, restore returns `recovery_required`; it does not guess a larger
host policy. Declared limits that cannot fit even an empty result, or exceed
the retained policy, are rejected as checkpoint conflicts.

Compatibility has an explicit limit: an old snapshot can contain an outcome
whose raw output was legal but whose escaped JSON or metadata exceeds the new
payload bound. Such a snapshot is rejected before commit with `limit_exceeded`,
without deleting or truncating evidence. This increment does not provide an
artifact migration for that historical content and does not claim all legacy
snapshots can resume unchanged.

Independent review found the missing durable legacy upgrade and acceptance of
an unusably small restored bound; both were repaired. It also identified two
existing internally generated denial messages that could exceed a legal
one-byte output limit. These now retain an empty denied output, with the reason
in the admission event or the restored workspace-change fact. Regressions cover
both paths followed by another process restoration.

This establishes bounded tool replies needed for capacity accounting. It does
not reserve aggregate checkpoint/wire space for all pending outcomes,
cancellation, pairing, child conclusions or terminal records. Provider outputs
also need a separate byte-bound/artifact strategy. C4 capacity and fault work,
C5/C6, the full acceptance audit and final independent review remain required.

The preceding activity-handoff commit `f539460b` completed remote CI successfully
on Linux, macOS and Windows (run `37053855510`).

Validation and review for this increment:

- Rust 1.99.0 workspace/all-feature nextest: 3932 passed, 22 skipped in
  122.460 seconds with four threads, including all ten payload integrations.
- The first workspace run stopped after a PTY resize/display wait timeout
  (`code_detached_backlog_catches_up_once_after_resizes`). The unchanged test
  passed alone in 4.713 seconds; the complete unchanged suite then passed.
- Strict workspace/all-target/all-feature clippy passed (36.17 seconds), as did
  strict workspace rustdoc (20.57 seconds), workspace doctests, format and diff
  checks. Rust 1.93.0 workspace/all-feature compilation passed (18.02 seconds).
- Independent read-only re-review found no remaining P1/P2 for this increment.
  Aggregate cleanup reservation and historical oversized-evidence migration
  remain open requirements, not covered by that review conclusion.

## C4 cleanup projection and recovery archives (in progress)

Admission now checks a conservative cancellation projection in addition to the
actual candidate. It applies both the host and caller-lowered checkpoint/wire
bounds, including base64 and envelope overhead. The projection accounts for
frozen tool result/status bodies, operation receipts, canonical pairing,
context-source copies, child conclusions, runtime wait results, terminal
records and ownership release. The real pairing implementation is shared with
the projection; fabricated projection values never enter committed state or
authorize execution. Near-full historical snapshots that cannot fit cleanup
are rejected before a new restore checkpoint.

Review exposed a recovery-specific flaw in the first projection: appending a
handoff while reserving another handoff regenerated the same obligation.
Repeated running observations also retained additional evidence copies. The
current implementation can move complete recovery histories to one immutable
`application/vnd.bitrouter.recovery+json` artifact. Its content-addressed root
occupies a reserved, fixed-size checkpoint slot. Once enabled for a session,
subsequent checkpoints keep the representation. No result, observation or
operation receipt is discarded. The scheduler retains the complete state;
restoration hydrates the archive before applying the existing history and
lifecycle validations. Full historical operation receipts remain inline.

The archive binds schema, session/run identities, invocation/attempt identities,
observation revisions and exact dependency references. Chunk staging precedes
the checkpoint ACK; the harness must validate and retain the root's transitive
dependencies atomically with the checkpoint. `HarnessPort::read_artifact` is
required and returns bounded ranges. Missing or corrupt roots/dependencies
reject recovery. Same-owner retransmission reconstructs the same archive from
the retained candidate and resubmits the original batch. Restore validates the
ownership chain and durable head before artifact I/O.

`RestorationActivity::started_at()` exposes entry to restoration. When installing
the lifecycle observer, the harness replays locally captured stops from that
instant, including stops during archive reads. The final lifecycle drain is
unchanged. Archive write admission and restoration use the same total bound:
the smaller of the manifest artifact quota and host unacknowledged-byte limit.
This is a bounded full-hydration implementation; it does not establish unlimited
archive retention or streaming replay of arbitrarily large historical state.

Initial validation passed all 279 orchestrator tests. The saturation regression
then additionally passed sixteen running recoveries, direct-root availability,
complete evidence retention, missing root/dependency rejection, irreversible
phase validation, both restore ACK-loss outcomes, and a stop captured during a
1.1-second archive-read wait. Workspace validation and final review are pending.

Remaining A20/C4 work is explicit: durable capacity-failure cancellation and
stopping new work, reserved artifact/storage capacity at exhaustion, byte
admission for unknown provider/preparation outputs, oversized legacy-result
migration, wider child/wait saturation cases and the remaining fault matrix.
This increment does not establish the full cleanup invariant across those
paths. C5/C6, A01–A23 and final independent acceptance remain required.

Validation of this increment with Rust 1.99.0:

- Workspace/all-feature nextest: 3935 passed, 22 skipped, in 237.087 seconds
  with four test threads. One native continuation/child-context test was slow
  (101.677 seconds) and passed.
- Strict workspace/all-target/all-feature clippy passed in 1m 19s. Strict
  workspace rustdoc passed in 39.75 seconds. Doctests: 5 passed, 1 ignored.
- Rust 1.93.0 workspace/all-feature check passed in 34.16 seconds; formatting
  and diff checks passed.
- The initial workspace compile identified two application test fixtures
  missing the newly required artifact-read method. They were corrected before
  the complete passing run.
- Final independent read-only review found no remaining P1/P2 within this
  increment. A dedicated same-owner archived-pending retransmission integration
  remains part of the fault matrix; current evidence for that branch is the
  deterministic pending-state implementation and chunk-idempotency contract test.

The preceding tool-payload commit `8f4edc12` completed remote CI successfully
(run `37058260630`). New-commit CI remains a separate gate.


## C4 durable checkpoint-capacity failure

A live transition that exceeds actual or projected checkpoint/wire capacity now
commits `run.capacity_reached` from the last acknowledged snapshot. The rejected
candidate, its operation receipt and any unaccepted tool result are discarded.
The replacement stores a canonical `LimitExceeded` resource error and the new
`RootRun.resource_constraint = checkpoint_capacity`, cancels live turns without
clearing uncertainty, pauses the root queue and includes atomic tool-start
fences. The same cleanup/result-pairing path used for time exhaustion preserves
late outcomes and ultimately reports a failed run. Active-time failures record
`active_time`; legacy failures without a cause retain their old clock meaning.

The caller receives `limit_exceeded / not_committed` after the failure ACK:
only the independent failure transition advanced the durable head. A matching
operation receipt is still required to consider the original input/result
accepted. Missing ACKs remain `unknown`. A replacement that cannot be prepared
fails closed. Restoration does not substitute a failure for its required
ownership/activity checkpoint; insufficient historical cleanup capacity rejects
before committing restoration.

Independent review found two defects in the initial implementation. Discarded
signals/cancel/interrupt candidates could strand their provisional dispatch
blocks, and history validation only remembered the immediately previous run.
The pending transition now retains precisely the blocks resolved by the
rejected candidate. The matching normal ACK, reconnect adoption or exact batch
retransmission clears those blocks; unrelated pending controls stay blocked.
Recovery retains failure facts by run identity across all supplied checkpoints,
checks event/cause/payload agreement, rejects erasure/rewrite/repeated failure,
and requires cancelling or recovery-required state for nonterminal failed
turns. A checkpoint-only anchor can contain a previously accepted failure.

Regression coverage includes normal failure ACK and both lost-ACK outcomes
with a running tool and a pending approval, exact retransmission, cancellation
delivery, fenced approval, actual/NotExecuted outcomes and terminal failure.
An abandoned failure request reconciles its pending identity while preserving
a distinct provisional control block until that operation is resolved.
Journal mutations exercise erasure, changed reason/cause/event, repeated
failure, removed cancellation, runnable resurrection, and intervening empty or
other-run snapshots. Existing saturated cleanup, full-sized outcomes, repeated
running recovery, unknown effects and active-time cases remain required checks.
Final workspace validation is recorded below when complete.

This increment does not reserve artifact/storage capacity at final exhaustion,
admit arbitrarily large provider/preparation output, migrate oversized legacy
results, or complete child/wait saturation and the remaining recovery fault
matrix. C4/A20, C5/C6 and the full acceptance audit remain open. The preceding
cleanup/archive commit `9eba26ed` completed remote CI successfully (run
`37066182902`); new-commit CI is a separate gate.


Validation of the durable capacity-failure increment with Rust 1.99.0:

- Workspace/all-feature nextest: 3938 passed, 22 skipped, in 115.075 seconds
  with four test threads, including all three new capacity-failure integrations
  and the existing saturation, recovery and active-time regressions.
- Strict workspace/all-target/all-feature clippy passed in 34.13 seconds;
  strict workspace rustdoc passed in 19.54 seconds. Workspace doctests:
  5 passed, 1 ignored.
- Rust 1.93.0 workspace/all-feature check passed in 20.63 seconds; formatting
  and diff checks passed.
- Final independent read-only review found no remaining P1/P2 for this
  increment. The full workspace run above was restarted after the preceding
  process handle and temporary log became unavailable; that interrupted run
  is not counted as validation evidence.

New-commit remote CI and the remaining C4–C6/full-acceptance work are separate
required gates.

## C4 artifact-body bounds and retained-state admission

New workspace and verification intents freeze `ToolResultLimits.artifact_bytes`
alongside output/payload limits. Every result or lifecycle observation counts
distinct referenced body bytes; identical references count once, conflicting
identities and overflowing totals reject before acceptance. The allowance uses
the manifest quota divided by `5 * run.outstanding_tools + 2`. It cannot grow
after signals or ownership changes. The two extra shares are an allocation
policy, not a separate physical-storage or future-archive guarantee.

Before ACK, admission adds actual distinct object sizes to unconsumed allowances
for first running/stopped/unknown observations and uncertain/definite results
of every unfinished bounded invocation. Hydrated recovery dependencies and
advertised material objects count even when the wire inventory is smaller.
Archive space uses the larger of the distinct acknowledged/replacement root
sizes and twice the current archive representation, including when still inline.
Exhaustion discards the candidate and uses the independent durable
`run.capacity_reached` transition; accepted evidence is not truncated or erased.

Independent review found two compatibility defects in the initial migration:
it narrowed an already dispatched legacy reply to the new allocation, and
required discarded run limits for a completed legacy child. Restoration now
preserves absent body limits. Legacy evidence still receives payload/reference,
availability and actual aggregate-quota checks, but no retroactive body-space
reservation is claimed. Existing payload-limit migration retains its original
provenance requirements; a previously frozen payload contract does not acquire
new run-policy requirements merely because its artifact field is absent.

Focused regressions cover frozen body limits after quota enlargement, duplicate
and conflicting references, overflow, one and two concurrent tools through
logical quota exhaustion and complete uncertain/definite recovery, a legacy
result larger than the new share, and a completed legacy child with default
input limits across root replacement. The focused artifact/payload/capacity
suite passed 19 tests. Final workspace checks and review follow below.

The first workspace run caught an error-priority regression for an oversized
payload containing an invalid artifact digest. Complete payload admission again
precedes artifact validation; the unchanged regression and all 286 orchestrator
tests then passed. Strict clippy also required boxing the enlarged
`ServerMessage::ToolExecute` variant; its JSON representation is unchanged.
An additional regression reproduced rejection of a valid small-input text-only
task by the hoisted reply-limit check. Reply admission errors are now applied
only when constructing an actual workspace invocation; plain or interrupted
output does not need to fit an unused tool-result envelope.

This increment does not complete A20/C4: physical storage reservation, staging
and historical-checkpoint retention, all future archive growth, legacy-body
reservations, oversized historical-result migration, unknown provider/output
admission, wider child/wait saturation and the remaining fault matrix remain.
C5/C6 and the final A01–A23 audit also remain open. Prior commit `599c131a`
completed remote CI successfully (run `37091394067`).

Final validation of this increment with Rust 1.99.0:

- Workspace/all-feature nextest: 3943 passed, 22 skipped, in 107.311 seconds
  with four test threads, including the final text-only admission regression.
  The complete orchestrator suite separately passed all 287 tests.
- Strict workspace/all-target/all-feature clippy passed in 30.53 seconds;
  strict workspace rustdoc passed in 18.20 seconds. Workspace doctests:
  5 passed, 1 ignored.
- Rust 1.93.0 workspace/all-feature check passed in 18.42 seconds; formatting
  and diff checks passed.
- Final independent read-only review found no remaining P1/P2 within this
  increment, including the compatibility fixes, validation priority, boxed
  message variant and deferred reply admission. The remaining C4/C5/C6 and
  full acceptance obligations above are unchanged. New-commit CI is a separate
  gate.

## C5 durable response exchanges (in progress)

`CoreSession::start_response` accepts a root input and a core-owned response ID
in one acknowledged checkpoint. `continue_response` creates a successor for
the latest completed response of the same unfinished run; duplicate operation
IDs recover the same receipt. A continuation requires a newer durable result
or control revision. `response` reads retained state, and `drive_response` uses
the existing scheduler, commits the exchange boundary, then releases eligible
tools. Ordinary `start`/`drive` retain their in-process behavior.

An exchange can be completed while its run is waiting. Results still enter
the existing `tool_result` operation, but ordinary `drive` does not consume
them or resume model work for a completed managed exchange. An explicit new
exchange enables that work. Output carries step, turn, agent ID and display
path attribution; the frozen pending-call map binds public call IDs to exact
invocations and attempts. These retained messages are internal core data,
not an implemented Responses wire projection or encrypted provider state.

Managed `ToolExecute.response_id` identifies its originating exchange. The
dispatch gate requires that exchange's committed completion and pending-call
authorization, in addition to the existing intent, epoch, permission and
start-fence checks. Verification calls use the same gate. The test harness
independently checks the durable terminal mapping before executing a command.
Completion reserves snapshot/wire capacity before output admission. Abandoned
consumers and lost completion ACKs retain the original pending batch for
same-owner reconciliation; process restoration preserves completed exchanges
while reauthorizing only the current unstarted invocation's epoch/event fence.

Recovery rejects rewritten/erased response records, changed completed output,
missing acceptance/completion events, mismatched call authority, and reported
execution before exchange completion. Ownership release waits for an open
exchange to close. A regression also exposed complete failed model attempts
left unsettled; failure now settles a step only when all admitted provider and
preparation evidence is complete, allowing the failed session to release.

The eleven exchange tests cover root/child attribution, verification,
failure, immutable continuation, ACK loss on either side of persistence,
consumer abandonment, process restoration before/after completion and forged
history. Independent review found that abandoning a driver after an attempt's
complete receipt but before output application could otherwise close a
recovery-required exchange prematurely. Completion now rejects any unsettled
model step. A held hop-end callback reproduces the exact boundary; both retry
paths keep the exchange open, and reconnect applies its output with the original
response authorization intact. All eleven targeted tests pass. The earlier
orchestrator suite passed 297 tests; final workspace validation and independent
review are recorded below when completed.

This implements the in-process foundation for C5, not its public transport.
Managed HTTP/SSE DTOs and streaming, exact HTTP/channel result normalization,
authenticated ownership/session registry, capabilities/channel routes and
independent remote-client conformance remain required. Remaining C4 storage,
output admission and fault work, C6 and the full A01–A23 audit remain open.

Prior commit `4b0ad40f` passed macOS/Windows tests, Linux workspace tests,
clippy, docs and MSRV in CI run `37132142243`, but its Linux interface shard
failed in an existing host bind-failure fixture. The fixture released sampled
ports before allocating its occupied socket, allowing that socket to reuse
the inference port during the control-collision case. It now reserves the
occupied port first; no production listener behavior was changed.

Final-source workspace/all-feature nextest passed 3954 tests, with 22 skipped,
in 107.697 seconds using four test threads. This run includes the final
unapplied-output recovery regression and the corrected listener fixture. Final
independent read-only review confirmed the recovery fix and found no remaining
P1/P2 within this increment; full C5/C6 and cross-stage acceptance remain open.

Strict workspace/all-target/all-feature clippy passed in 30.78 seconds. Strict
rustdoc passed in 18.44 seconds after correcting the new module's URL markup;
workspace doctests passed 5 tests with 1 ignored. Rust 1.93.0 workspace/all-feature
check passed in 18.54 seconds. Formatting and diff checks passed. New-commit
remote CI is tracked separately from these local results.


## C5 remote transport implementation in progress

The service host now wraps ordinary inference with an explicitly negotiated
managed Responses adapter and authenticated capabilities/channel routes. A
virtual-key principal owns each registry entry, even with ordinary skip-auth
configuration. Authentication headers remain volatile; model execution invokes
the shared pipeline with those headers so key-bound policy is not lost through
the trusted in-process caller shortcut. HTTP disconnect does not own execution:
a bounded per-session job retains acceptance/driver work and duplicate consumers
attach to that operation. Same-owner channel reconnect uses core head/batch
reconciliation; I/O timeouts close the channel and clear transport waiters.

`continue_response_with_results` atomically commits the successor response and
all new result receipts, with individually attributed `tool.result` events.
HTTP items name the same result operation IDs as channel delivery. Exact replay
returns the original receipt; changed content, public-ID/attempt mismatches and
oversized aggregate input fail before partial acceptance. The aggregate obeys
the frozen run limit as well as the session bound. Durable response projection
retains creation time, frozen input, provider/public-call mappings, final answer
and attributed collaboration events; completed projection records are immutable.

`TaskInput.max_concurrent_subagents` limits active descendant turns across the
tree, including waits, independently of model slots and retained agent count.
Finished-context followups await a slot. A descendant cannot queue work onto an
idle agent while every slot is occupied, because its assigned-work dependency
would otherwise prevent its own slot from being released. Restore checks the
active count against the frozen input. An admitted descendant-owned queued
followup reserves the target's next slot, so a subsequent root spawn cannot
steal it and form a wait cycle.

Read-only stage review found the followup capacity cycle and missing aggregate
run input bound; regression tests accompany both fixes. The independent transport
review found and fixed initialization cancellation before the first restoration
proposal, lost restoration ACKs, a mutating head query and missing initial
driver wakeup. Binding/restoration registration retains the prepared session
and original initialization event before any interruptible checkpoint wait.
Reconnect finishes this event before its own checkpoint; replacement transfers
retained provider evidence before admitting execution. The cleanup projection
also reserves response-retained child-delivery events alongside mailbox copies.
The implementation is committed as `d6e5f2bf`. Loopback tests
exercise the production model pipeline through independent HTTP and
WebSocket clients, cross-owner rejection, completion ACK before tool delivery,
channel/HTTP duplicate result delivery, abandoned SSE consumers and ordinary
inference isolation. These are not live-provider or production-harness evidence.

The final targeted run passed 25 tests (4.883 seconds), including seven remote
API tests, sixteen response/core continuation tests, an exact-capacity child
delivery regression, and cancellation during restore registration followed by
full-journal restoration. Final-answer projection covers multipart text and
verification success/failure without duplicate or provisional final output.
The two focused independent re-reviews found no remaining P1/P2 in those fixes.
SSE output deltas are currently buffered until exchange completion. Broader
transport/authentication/pressure conformance remains required.

Remaining C5 work includes the measured remote restoration clock bridge for
running tools, authenticated takeover/release/recovery conformance, policy/key
revocation and full output/transport pressure coverage. C4 storage/output
admission work, C6 and the complete A01–A23 audit remain open. The earlier head's
CI 37135530717 failed a Windows multi-step collaboration fixture's five-second
wall-clock watchdog; that fixture now has a sixty-second deadlock guard while
keeping all scheduling/attribution assertions and production budgets unchanged.

Final-source workspace/all-feature nextest passed 3968 tests with 22 skipped
in 109.301 seconds using four test threads. The first run stopped because an
earlier fixture had generated runtime identity files in the repository test
directory; the fixture now uses a temporary runtime home, and the old files
were preserved outside that directory before the complete successful rerun.
Strict workspace/all-target/all-feature clippy passed in 29.78 seconds after
collapsing a nested conditional. Formatting and diff checks passed; strict
rustdoc passed in 27.99 seconds, workspace doctests passed 5 tests with 1 ignored,
and Rust 1.93.0 workspace/all-feature check passed in 24.81 seconds. Commit
`d6e5f2bf` passed every job of remote CI `37142668620`, including
Linux/macOS/Windows tests.

## C5 authentication and HTTP consumer lifetime

Independent HTTP/WebSocket clients now verify that a key-bound model policy
rejects managed execution before any upstream preparation, revoked/expired
credentials reject new HTTP/upgrade requests and work on an existing channel,
and a live policy reload is applied on the next response exchange. Replaying
an earlier success or failure keeps its original response even after policy
changes; replay does not rerun model preparation or generation.

HTTP consumer admission now follows the response body and every emitted byte
chunk, including downstream clones. Previously a non-streaming handler released
its permit before the network finished retaining its body; an SSE body could
also reach EOF while its final frame remained buffered. A shared byte owner
keeps the permit until the final retained chunk is dropped. Consumer loss still
does not cancel accepted core execution. The ownership regression exercises EOF,
body drop, two retained chunks and a cloned final chunk.

The ten remote API tests and this ownership regression passed (11 total,
2.557 seconds). Independent read-only review found no P1/P2 in this increment.
Final-source workspace/all-feature nextest passed 3972 tests with 22 skipped
in 110.366 seconds using four test threads. Strict workspace/all-target/
all-feature clippy passed in 24.70 seconds, strict rustdoc in 17.31 seconds,
and Rust 1.93.0 workspace/all-feature check in 16.48 seconds. Workspace doctests
passed five tests with one ignored; formatting and diff checks passed.
Previous head `d6e5f2bf` passed every job of CI `37142668620`, including
Linux/macOS/Windows tests. The new commit's remote CI remains a separate gate.

This closes these specific authentication and consumer-lifetime cases, not the
entire transport pressure requirement. HTTP currently materializes response
projection, and SSE eagerly queues multiple copies of output values without a
shared per-session byte admission ledger. That path still needs incremental
serialization/projection and pressure tests. Mid-operation authorization races,
authenticated replacement/release conformance, measured remote running-tool
clock handoff, remaining C4/C6 work and full acceptance remain open.

## C5 incremental output and slow-consumer isolation

Managed HTTP consumers now share an immutable terminal exchange instead of
cloning it per reader. Borrowed wire views serialize JSON and SSE directly into
admitted chunks of at most 16 KiB. Text, tool arguments, attributed events and
the final response are not copied into queued JSON values. Nested verification
arguments stream through JSON-string escaping. A complete response or logical
SSE event can exceed the byte window without truncation or rejected output.

The session ledger accounts for allocated chunk capacity and retains ownership
through downstream `Bytes` clones, EOF and body drop. Frozen run limits apply
across consumers of that run. A producer waiting for capacity exits when its
consumer closes; accepted core work remains independent. At most sixteen
admitted HTTP consumers can own producers. Weak ledgers preserve outstanding
bytes across restoration, release and rebind; a lower limit that cannot yet
contain retained bytes returns busy before binding side effects.

All current harness `ServerMessage` variants are control or durable traffic.
They use a separate bounded staging lane under `unacknowledged_bytes`, leaving
the `ephemeral_bytes` window for HTTP/UI output. One encoded WebSocket message
is admitted before allocation and retained through socket flush. Both split
socket halves retain its owner on failure/abort because the underlying socket
may have copied it into its own write buffer. Existing core checkpoint/ACK
accounting still owns pending durable batches; this is not a completed proof of
aggregate physical source, checkpoint and transport-copy memory accounting.

Managed ingress reserves its consumer slot before reading a body, authenticates,
and applies the host input bound before deserialization. No-beta ordinary
Responses inspection retains its original 16 MiB bound behind four separate
host slots. Both reads have a twenty-second deadline, and managed requests
require positive output capacity. The ordinary inference handler remains
available when managed consumer admission is full.

Independent review identified and verified fixes for control starvation behind
UI bytes, WebSocket copied-buffer ownership on split-socket failure, an ordinary
ingress slot with no deadline, and typed SSE projection errors becoming body
errors. Final focused reviews found no remaining P1/P2 in this increment.
The final targeted run passed 22 tests in 2.214 seconds. These include concurrent
JSON/SSE clients receiving escaped output larger than 4 KiB session/1 KiB run
windows, a held server body chunk alongside real WebSocket head/cancel commands,
17-byte nested-argument serialization, EOF/cloned-chunk ownership, consumer
cancellation while awaiting bytes, failed-writer cleanup, typed unsupported
output errors, and admission before polling a request body.

Final-source workspace/all-feature nextest passed 3983 tests with 22 skipped
in 109.972 seconds using four test threads. Strict workspace/all-target/
all-feature clippy passed in 24.49 seconds, strict rustdoc in 17.32 seconds,
and Rust 1.93.0 workspace/all-feature check in 16.24 seconds. Workspace doctests
passed five tests with one ignored; formatting, diff and tracked-ignore checks
passed. This evidence does not complete physical memory accounting,
maximum-size control/ACK-loss stress, remote clock handoff, remaining
C4/C6 work or A01–A23 acceptance. Previous head `3dbea06c` passed all jobs of
remote CI `37144726860`; the next commit's CI is tracked separately.


## C5 remote ownership and recovery conformance

An independent HTTP/WebSocket client retains exact checkpoint bytes and controls
ACK delivery. Initial binding and ownership release are each interrupted before
and after persistence. Reconnection uses the actual durable head, preserves the
original proposal exactly once, and never renews a released grant. The release
receipt remains queryable after its lost ACK; neither HTTP input nor channel
queue resumption can start work under that released owner.

A terminal session is released and restored under the next epoch. Restoring its
released epoch fails; stale HTTP input and WebSocket mutation envelopes fail
without advancing the replacement's head. The original response replays unchanged
under the new binding, and an explicit resume/new input completes another run.
A separately held HTTP provider proves that a head query leaves the durable head
unchanged and the original request completes with exactly one provider call.
The provider is held by an explicit permit rather than a wall-clock delay.

- Targeted managed API suite: 17 passed, 2.508 seconds, four test threads.
- The four new regressions use the shipped application/authentication pipeline
  and loopback network clients. They add recovery evidence; they do not establish
  a production harness, a live provider, running-tool timing handoff, or the
  complete A12–A17 acceptance matrix.
- Independent read-only review found no actionable P1/P2 in this increment.
- Rust 1.99.0 workspace/all-feature nextest: 3987 passed, 22 skipped, 108.534
  seconds, four test threads. Strict workspace/all-target/all-feature clippy
  passed (1.50 seconds); strict rustdoc passed (8.61 seconds).
- Workspace doctests: five passed, one ignored. Rust 1.93.0 workspace/all-feature
  check passed (0.51 seconds); formatting, diff and tracked-ignore checks passed.
- Previous head `a66aa7ef` passed all jobs of GitHub CI 37147783376. The CI for
  this increment remains a separate gate.

## C4 managed provider response ingress

Managed model steps pass the smaller of the frozen root run's and session's
`checkpoint_bytes` through the SDK control wrapper to the HTTP executor. Each
response is bounded before complete JSON buffering or SSE event parsing. The
limit counts cumulative decoded entity bytes, including SSE framing, keepalives,
deltas and terminal data. Content-Length can reject early but cannot replace
actual chunk accounting. Input-count responses use the smaller of this limit
and their existing 16 KiB bound. Ordinary uncontrolled execution keeps its
existing text decoding behavior.

Oversized successful responses produce a bounded failure receipt, retain
unknown cost exposure when usage is unavailable, and cannot authorize tools.
Oversized error bodies are discarded without losing the original HTTP status:
400/401/403 do not become fallback-eligible invalid responses, 401 still permits
the existing single authentication refresh, and 429 retains Retry-After.
Transport errors retain their existing classification. The admission check
also stops chunked responses that never send EOF and unfinished SSE events.

The seven SDK regressions passed in 0.039 seconds and all nineteen managed API
tests passed in 2.953 seconds. Independent review identified the error-status
regression and verified its fix; the focused re-review found no remaining
P1/P2. This increment bounds provider HTTP entity ingress; full physical copy
accounting, custom executors, preparation hooks, canonical output admission and
remaining storage/acceptance work remain open.

CI `37149428807` for previous head `1a85743d` failed only the Windows tests:
the new recovery client's explicit close drain received OS error 10053
(`ConnectionAborted`). That cleanup now accepts connection abort/reset after
close initiation. Active send/receive errors, epoch assertions and the close
timeout remain unchanged. Independent review found no P1/P2 in this fix;
new-head Windows CI must confirm it.

Final-source Rust 1.99.0 workspace/all-feature nextest passed 3996 tests with
22 skipped in 111.394 seconds using four test threads. Strict workspace/all-
target/all-feature clippy passed in 39.632 seconds, strict rustdoc in 26.123
seconds, and Rust 1.93.0 workspace/all-feature check in 23.358 seconds.
Workspace doctests passed five tests with one ignored. Formatting, diff and
tracked-ignore checks passed. These gates and the independent reviews cover
this increment; the remaining acceptance requirements are still open.

## C5 authorization after admission and delivery waits

`HarnessPort::authorize_dispatch` lets credential-bound hosts check current
authority after durable admission waits, before preparation callbacks, input
counting, model attempts, provider integration phases and tool dispatch. The
trusted in-process default preserves existing embeddings. A rejected check
disconnects the session without erasing accepted intents, output or accounting;
recovering them still requires exact head reconciliation. The hook runs under
the admission lock and must not invoke session mutations.

The remote port checks its live virtual key and fences its channel on denial
or timeout. The socket writer checks again after byte/queue/prior-flush waits,
immediately before sending the next message. Managed HTTP rechecks after body
upload and session lookup, including immutable response replay. Previously
these paths could continue waiting after their last credential check and
dispatch using an expired decision. Disconnect also interrupts an authorization
check already in progress.

Core regressions revoke authority while acknowledged preparation, count,
attempt, provider dispatch/retry and tool-output checkpoints are held. They
verify that callbacks, actual provider HTTP calls and tool sends remain blocked
after the ACK returns while accepted state is retained. API regressions use
persisted key revocation/expiry between output queueing and socket delivery,
and revoke during held uploads for new input and completed-response replay.
These dispatch checks cannot retract I/O already started and do not establish
instantaneous cancellation of an in-flight provider or its external effects.
Running-tool remote clock handoff, remaining output/storage admission,
process-replacement conformance and full C6/A01–A23 acceptance remain open.

Independent review found cached response replay could wait on the session
registry after its last check and bypass the new-job worker's authorization.
Cached progress is now authorized after releasing that lock, and the current
entry's readiness/epoch is rechecked before selecting a job. New work takes
the current registered session rather than a handle captured before waiting.
Regressions hold the registry while revoking/expiring the key or changing the
epoch. The earlier 4004-test workspace pass predates this repair; final-source
validation is recorded separately.

Final-source Rust 1.99.0 workspace/all-feature nextest passed 4006 tests with
22 skipped in 113.211 seconds using four test threads. The final targeted
API/channel suite passed 26 tests in 3.111 seconds. Strict workspace/all-target/
all-feature clippy passed in 25.078 seconds, strict rustdoc in 17.317 seconds,
and Rust 1.93.0 workspace/all-feature check in 16.718 seconds. Workspace doctests
passed five tests with one ignored; formatting, diff and tracked-ignore checks
passed. Independent code and documentation re-review found no remaining P1/P2
in this increment. Previous head `feef358d` passed all jobs of CI `37151419610`,
including Windows tests; new-head CI is a separate gate.

## C2 committed cancellation of live provider calls

Each model step now has a cancellation token scoped to its agent turn and
inherited from the current connection. Adopting an acknowledged checkpoint
stops executor futures whose matching run/turn requested cancellation. The
same check runs when registering a new control, covering a budget cancellation
that commits before registration. A child cancellation never cancels its
connection, parent or sibling controls. Pending cancellation fences new
dispatch without stopping accepted I/O; definitive rejection releases the
request's provisional barrier.

The SDK's existing detached execution still records the attempt and completes
settlement. Complete output already returned to the SDK retains its receipt
and usage; incomplete calls retain unknown provider exposure. Cancellation
does not synthesize a completed provider integration phase or a zero-cost
receipt. Preparation/counting callbacks keep their existing settlement gates;
this increment changes cancellation of the provider executor, not arbitrary
extension callback lifetimes.

Regressions hold root, child, grandchild and sibling executor futures at once,
reject a stale interrupt, then hold the real interrupt ACK. Only the child and
grandchild stop after acknowledgement, without fallback; root and sibling
complete. Actual HTTP and bridged streaming calls stop before response headers
without waiting for a delayed server, while SDK settlement runs once and cost
remains unknown. The complete-output regression now holds an already received
child result at its outcome ACK, proving cancellation preserves known usage
without applying new effects. Budget regressions now hold the interrupted
attempt's outcome ACK, confirming cleanup waits for detached SDK settlement.
Thirteen targeted regressions passed in 5.227 seconds. Independent review
found no actionable P1/P2. Final-source validation is recorded below.

Previous-head CI `37153208433` failed only a Windows lifecycle test: all three
attempts exhausted its five-second watchdog while running multiple durable
follow-up turns. That functional test now uses the same thirty-second bounded
watchdog as other lifecycle tests; its FIFO, identity, wait and reuse assertions
are unchanged. New-head Windows CI must confirm the adjustment.

Final-source Rust 1.99.0 workspace/all-feature nextest passed 4008 tests with
22 skipped in 112.993 seconds using four test threads. Strict workspace/all-
target/all-feature clippy passed in 32.361 seconds, strict rustdoc in 19.971
seconds, and Rust 1.93.0 workspace/all-feature check in 19.363 seconds.
Workspace doctests passed five tests with one ignored. Formatting, diff and
tracked-ignore checks passed. Independent final re-review of code, tests,
documentation and the Windows watchdog adjustment found no actionable P1/P2.
The complete C4–C6 and A01–A23 acceptance requirements remain open. Commit
`dff74045` passed CI `37154689564`, including Linux, macOS and Windows tests.

## C4 canonical model output admission

Managed execution now counts serialized canonical result bytes before private
output sealing and durable-report cloning, including custom executor output.
It checks again after sealing to include newly attached policy metadata. The
initial bound was the minimum of the frozen root run and session checkpoint
limits; the prospective reservation increment below replaces it for new
attempts. The provider HTTP entity bound remains a separate ingress check.
Counting stops at the bound without building an encoded copy and includes JSON
escaping.

A complete rejected result has a bounded `output_rejection` summary with
canonical usage counters and provenance. It does not carry result content or
raw provider metadata. The cost estimator sees the original result before
projection, and SDK settlement retains the original execution and raw usage.
The error is returned after settlement, without another provider fallback.
Core preserves the receipt but cannot derive tools or success from its content.

Restoration and same-owner reconnect recognize the durable rejection even if
the outcome ACK was lost before or after persistence. They settle the rejected
step as failed instead of treating it as an uncertain attempt eligible for
retry. A restored root failure cancels descendants as the live failure path
does. Cancellation and steering retain their existing precedence.

Independent review identified three gaps, now covered by regressions. Rejected
output bypasses fallible execution success hooks so the original billed usage
reaches settlement even when such a hook would fail. Reconnect also recognizes
rejection evidence buffered after disconnection and imported after the old step
was closed as interrupted. Late evidence cannot fail a replacement run, a newer
step, or input superseded by pending or already applied steering. Applied input
is identified by its durable revision even before its next model step exists.
The applied-steering regression reproduced the failure before the fix. Final
independent read-only review found no remaining P1/P2 in this increment. Nine
focused regressions passed in 0.393 seconds.

This increment bounds one canonical result and its durable projection. It does
not reserve every future history/archive byte, all concurrent pending outputs,
or allocations inside a custom executor, raw settlement data or trusted hooks.
Remote running-tool clock handoff and the remaining C4–C6/A01–A23 acceptance
requirements remain open. Final workspace validation is recorded below.

Final-source Rust 1.99.0 workspace/all-feature nextest passed 4015 tests with
22 skipped in 112.708 seconds using four test threads. Strict workspace/all-
target/all-feature clippy passed in 72.589 seconds, strict rustdoc in 29.496
seconds, and Rust 1.93.0 workspace/all-feature check in 98.572 seconds.
Workspace doctests passed five tests with one ignored. Formatting, diff and
tracked-ignore checks passed. Remote CI for the new commit is a separate gate.

Commit `58753028` passed CI `37157297097`.

## C4 prospective recovery archive admission

New ordinary and verification tool intents freeze a per-observation
`recovery_archive_allowance`. It includes the complete frozen JSON payload,
duplicated dependency metadata, archive entry/revision growth, and a
maximum-width authenticated activity handoff. Current archive bytes plus
unconsumed first running/stopped/unknown-effect allowances are checked against
the restoration bound and counted twice against artifact quota for replacement
overlap. This check precedes tool dispatch and applies before wire compaction.
Archive encoding itself counts bytes before allocating its serialized buffer.

Live observations do not consume archive allowances. First archived phases do;
definite outcomes release unused allowances. Restoration validates a retained
allowance against the original reply contract; absence remains a legacy marker
and never narrows an already authorized result. Repeated archive evidence still
needs fresh room. Artifact-body reservations remain separate: a fresh artifact
for a phase already reported live does not acquire a second body allowance.

Regression coverage includes low-quota rejection before tool dispatch, legacy
restoration and forged allowances. The saturation test first creates a real
archive, fills the logical artifact quota, then imports full-payload/full-body
stopped and uncertain observations for two outstanding tools. Restore ACK loss
is exercised before and after persistence; reconnect preserves the exact
proposal and never redispatches the tools. Definite results then permit cleanup.
Disabling only the prospective archive count reproduces rejection before the
restore proposal can commit. With the reservation enabled, the regression passes.
General concurrent-tool fixtures now advertise sufficient archive capacity;
dedicated quota tests retain explicit small limits.

Initial independent review found no new P1/P2 in the implementation. Final
review and workspace validation are recorded below. Physical storage/staging
leases, retained historical checkpoints, legacy migration, arbitrary repeated
restore growth, concurrent model-output headroom and the full C4–C6/A01–A23
requirements remain open.

Final-source Rust 1.99.0 workspace/all-feature nextest passed 4018 tests with
22 skipped in 114.939 seconds using four test threads. Strict workspace/all-
target/all-feature clippy passed in 32.401 seconds, strict rustdoc in 18.002
seconds, and Rust 1.93.0 workspace/all-feature check in 19.386 seconds.
Workspace doctests passed five tests with one ignored. Formatting, diff and
tracked-ignore checks passed. Final independent code/test/documentation review
found no new P1/P2. New-head remote CI remains a separate gate.

## C4 tree and wait cleanup under checkpoint saturation

The `saturated_tree_preserves_waits_pairing_and_child_delivery_after_ack_loss`
integration test drives a root, two children and a grandchild through real core
transitions. Four workspace tools remain outstanding after five model calls. A
model-originated wait and eight runtime waits are pending; each agent mailbox
is full and a child has two accepted follow-up assignments. Repeated optional
tool observations fill the checkpoint until core commits its independent
capacity-failure transition and rejects the unaccepted observation.

All four dispatched tools then submit their full frozen JSON payloads. The test
loses the first child-result delivery ACK both before and after persistence,
reconciles the original grant/head, and verifies exact-batch retransmission or
adoption. The tree reaches a failed run with all tool results paired into
history, one interruption result for the model wait, durable runtime wait
results with their original target identities, retained accepted operation
identities and pre-existing mail, and one attributed conclusion per child with
its context provenance. Queued follow-ups do not launch model work. Every
committed checkpoint and wire batch stays within the frozen bounds; settled
ownership release succeeds.

This covers a combined A06/A08/A12/A16/A20 fault scenario using the in-process
durable harness fixture. It does not establish full acceptance, maximum tree
width/depth pressure, unbounded history, physical storage admission, concurrent
provider output reservations or production harness conformance. Runtime waits
observe cancelling turns without final answers; completed-child answer and
source growth in wait results need separate coverage. No production contract
is relaxed by the test. Independent read-only review of the test and related
capacity, interruption, pairing, wait and notification paths found no P1/P2.
The fixture reports Running consistently with its already started tools.

Final-source workspace/all-feature nextest passed **4019 tests, 22 skipped**
in 121.428 seconds with four test threads; the new combined test passed in
25.146 seconds. Strict workspace/all-target/all-feature clippy passed in 6.477
seconds, strict rustdoc in 1.911 seconds, workspace doctests passed five tests
with one ignored, and Rust 1.93.0 workspace/all-feature check passed in 0.557
seconds. Formatting, diff and tracked-ignore checks passed. Previous head
`a989cb56` passed all jobs of CI 37177871654. New-head remote CI remains a
separate gate.

## C4 prospective canonical model output contributions

New attempts freeze `canonical_output_bytes` in their acknowledged intent before
provider dispatch. The allowance is the frozen root run's `checkpoint_bytes`
divided by `2 * (active_models + 1)`, rounded down. All agents use that policy.
The provider HTTP entity bound remains independent. Checkpoint cleanup admission
reserves twice each unresolved attempt's allowance for its canonical receipt and
outcome-event contributions; retained results replace that reservation with
actual candidate bytes. SDK checks before and after private-output sealing use
the frozen allowance, preserving oversized-result usage and settlement behavior.

The cost inventory stores the same contract in `ProviderAttemptSource`, keeping
an unresolved attempt's reservation after its root run or child turn retires.
Current attempt and inventory fields must agree. Late provider evidence is
validated against the original allowance before any state change; both result
bytes and a rejection's stated byte limit are checked. Restoration validates
retained policies and pending capacity before ownership takeover. Missing legacy
fields retain their earlier contract without a retrospective restriction.

Independent review found that scanning current turns alone lost the reservation
when an interrupted turn was replaced. Persistent inventory accounting fixes
that gap. Re-review found no new P1/P2. Regressions cover two overlapping model
calls returning their complete allowances, competing signals, cancelled-turn
settlement and release, forged/legacy checkpoints, insufficient takeover space,
and a retired attempt's complete late result after competing state saturates
capacity. Invalid late evidence leaves the head and operation inventory intact;
valid evidence is idempotent and does not rerun the provider.

This increment reserves canonical result contributions, not arbitrary report
metadata, later history/response/archive expansion, physical SDK copies or
custom executor/preparation-hook allocation. The concurrent-results test cancels
after both receipts commit and before history application; it does not prove
full-sized results can proceed through every later projection. Physical storage
leases, remote running-tool time handoff, production harness conformance and
complete C4–C6/A01–A23 acceptance remain open. Final validation is recorded below.

The retired-attempt regression passed in 2.506 seconds. A negative check disabled
only the cost inventory's reservation for attempts absent from current turns;
the same test then rejected valid late evidence with `checkpoint cleanup
capacity exhausted / not_committed`. Production source was restored before the
final workspace gates. This isolates the turn-retirement reservation gap.

Final-source Rust 1.99.0 workspace/all-feature nextest passed **4023 tests,
22 skipped** in 125.635 seconds using four test threads. Strict workspace/all-
target/all-feature clippy passed in 33.211 seconds, strict rustdoc in 18.152
seconds, and Rust 1.93.0 workspace/all-feature check in 19.397 seconds.
Workspace doctests passed five tests with one ignored. Formatting, diff and
tracked-ignore checks passed. Final independent code/test/documentation review
found no remaining actionable P1/P2 in this increment. Previous head `421a5196`
passed CI 37178987902; new-head remote CI remains a separate gate.


## C4 canonical output delivery after receipt admission

A complete result could fit its old frozen bound and commit its receipt while
its history, provisional answer and managed response exceeded checkpoint
capacity. A full-allowance text-result regression reproduced a failed response
where a completed response was expected.

New attempts now freeze `canonical_output_version: 2`. Its result bound is the
root's `checkpoint_bytes / [8 * (active_models + 1)]`, rounded down, leaving
room for delivery copies alongside receipt/event contributions. Pending
capacity includes history, provisional and root answers, child conclusions,
active Responses output/answer and terminal-event bytes. A received result
retains its actual delivery contribution until the step is applied or
interrupted; the existing cleanup projection then counts the retained state.
Retired unresolved attempts keep their receipt/event reservation through the
cost inventory. A missing version preserves the previous two-share policy;
missing both fields preserves earlier legacy behavior. Unknown policies and
attempt/inventory inconsistencies reject recovery without advancing its head.

Independent review found that runtime waits need two answer contributions for
the cleanup projection's before/after views, and both views newly expose all
retained context sources. The corrected reservation includes those sources,
not just a source introduced by the current step. A unit admission test uses
four waits and substantial retained sources, searches the exact host capacity
boundary with a fixed run policy, then applies the complete result and response
capture. This is projection arithmetic evidence, not a production wait workflow.

The managed-response regression returns the full frozen canonical allowance,
checks receipt/history/turn/run/response content, single settlement, replay and
release, and exercises output-application ACK loss before and after persistence
plus process restoration without another model call. Version tests cover the
old bounded contract, unbounded legacy records, unknown versions, missing bytes
and inconsistent inventory. Final validation is recorded below.

This work does not reserve every new tool intent, later model prompt, normal
model-originated wait expansion, arbitrary report metadata, recovery archive,
physical copy or trusted extension allocation. Existing independent admission
still governs those transitions. The complete C4–C6/A01–A23 requirements,
remote running-tool time handoff and production harness conformance remain open.

Nine focused tests passed in 9.366 seconds. Two surgical negative checks kept
the version-2 result limit unchanged: removing only delivery admission, and
restoring the single-wait-copy/missing-source bug. Both caused the exact-boundary
projection test to reject output with `checkpoint cleanup capacity exhausted`.
Production source was restored before final workspace validation.

Rust 1.99.0 workspace/all-feature nextest passed **4026 tests, 22 skipped** in
128.394 seconds using four test threads. A subsequent test-only `Option::map`
to `if let` lint correction passed its targeted regression in 0.182 seconds;
production code was unchanged. Final strict workspace/all-target/all-feature
clippy passed in 5.501 seconds, strict rustdoc in 18.231 seconds, workspace
doctests passed five tests with one ignored, and Rust 1.93.0 workspace/all-feature
check passed in 19.174 seconds. Formatting, diff and tracked-ignore checks passed.
Final independent code/test/documentation review found no remaining actionable
P1/P2 in this increment. Commit `abc628ab` passed CI 37181083836; new-head remote
CI remains a separate gate.


## C4 normal model-wait delivery and source propagation

New model-owned `wait_agent` intents freeze `wait_output_version: 1`. Admission
now forecasts a normal result as well as cancellation cleanup: the call result,
JSON-string tool history, durable event, Responses event and inherited context
sources. The history forecast shares the production renderer to preserve JSON
escaping. Two queue views cover answer visibility before and after cancellation;
source propagation counts parent conclusions, runtime waits and model-wait
consumers, visiting each receiving agent once while retaining separate calls.

Unapplied model outputs reserve future answer and source contributions. An
independent review identified another handoff: a verification tool can append a
large workspace revision after its model step has settled. Unknown tool sources
now retain downstream wait capacity through a definite receipt and until
canonical pairing. Completed but unpaired model-wait sources retain the same
obligation. Forecasts never become execution evidence or replace actual results.

Legacy intents without the marker keep their prior cleanup-only contract;
normal completion still requires ordinary admission. Restoration rejects unknown
versions, markers on non-wait actions and marker changes across the supplied
journal, without advancing ownership. New core intents retain the marker even
after their result is consumed. This is a checkpoint addition, not a new API
endpoint or a harness tool-execution requirement.

Boundary tests search the exact host limit while retaining the original run
policy. They exercise a full canonical child result, two model waits, four
runtime waits, substantial retained sources and Responses capture. A separate
verification regression fills the entire tool JSON payload with an escaped
workspace revision, then saturates again between receipt, source pairing and
wait completion. These tests validate reservation arithmetic. A real scheduling
test separately checks a full-allowance child result through model wait/history,
ACK loss before and after persistence, exact batch retransmission, single child
execution and settlement, cold response replay and release. Policy restoration
covers legacy snapshots, unknown versions, non-wait markers and journal removal.

New tool intents, later model prompts, full report metadata, future archive and
physical allocation obligations remain separate. Remote running-tool time
handoff, production harness integration and the complete C4–C6/A01–A23 acceptance
contract remain open. Final checks and independent review are recorded below.

Eleven focused tests passed in 25.265 seconds. Three surgical negative checks
kept all frozen result limits unchanged and separately disabled prospective
model contributions, known normal-wait forecasts, and incoming source
contributions. Each exact-boundary regression then failed with
`checkpoint cleanup capacity exhausted`; source was restored after every check.
The last test edit additionally verifies that a legacy pending wait remains
admissible at a boundary that cannot admit the new normal-wait obligation.
Final workspace validation follows. Previous head `3a91eef7` passed
CI 37182810873, including the platform test jobs.

The first workspace run passed 4028 tests but exposed one existing tree fixture
that could no longer admit its initial model wait under a 384 KiB checkpoint
limit. The failure preceded optional-capacity saturation. That fixture now
allows 640 KiB (1280 KiB wire) for the new normal-source obligation, then still
fills optional observations until the actual checkpoint-capacity failure. Full
replies, waits, mailboxes, ACK-loss assertions and every batch-size check remain.
Independent review confirmed that this adjustment preserves the saturation
requirement; final validation is recorded separately.

Final Rust 1.99.0 workspace/all-feature nextest passed **4029 tests, 22 skipped**
in 150.546 seconds with four test threads. Strict workspace/all-target/all-feature
clippy passed in 31.860 seconds, strict rustdoc in 18.382 seconds, workspace
doctests passed five tests with one ignored, and Rust 1.93.0 workspace/all-feature
check passed in 19.132 seconds. Formatting, diff and tracked-ignore checks
passed. Final independent review, including the adjusted saturation fixture,
found no remaining actionable P1/P2 in this increment. New-head remote CI
remains a separate gate; the full core acceptance goal remains incomplete.


## C4 complete provider-attempt report admission

An oversized provider error could previously disconnect durable authority while
recording its outcome. Canonical-result admission did not bound actual serving
identities, pricing metadata or error strings. Version 3 freezes a complete
`attempt_report_bytes` contract in each attempt and its retained cost inventory.
The canonical allowance uses sixteen shares per active-model slot plus one;
the report adds a rejection envelope for the known request and selected route.
Pre-dispatch admission covers the complete receipt/event, ledger, late evidence
and terminal-error contributions, while known errors retain their prospective
failure-delivery capacity through application. Versions 2 and unversioned
contracts keep their original semantics during restoration.

The SDK estimates cost from the original execution evidence first. A report
that exceeds its contract becomes an explicit terminal rejection summary with
versioned byte-count/SHA-256 commitments to the pre-projection report, actual
serving identities and pricing metadata. The report hash covers serde JSON after
canonical admission; it does not claim to hash raw provider wire bytes. The
summary does not preserve readable raw diagnostics. Canonical counters, cache
observations and zero/nonzero configured estimates retain their original meaning;
unknown cost remains unknown. Non-finite pricing rejects even a small report,
with exact floating-point bits retained independently of JSON serialization.

Success and failure rejections both stop fallback. Successful executor results
still reach SDK settlement with original usage and serving identities, including
when a fallible execution hook would otherwise discard them. Reconnect and cold
restoration treat the report as terminal without authorizing its tool content.
Retired evidence is checked against the original report contract. This does not
bound other preparation/callback reports, later prompts/tool intents, physical
allocations or cumulative archive storage. The complete core acceptance goal
and production harness integration remain open.

Twenty-one targeted tests passed, covering exact and over-limit reports, the
maximum rejection envelope, original settlement identities/raw usage, missing
envelope rejection before dispatch, zero/nonzero estimates, non-finite pricing,
outcome ACK loss and cold restoration, legacy policies and forged contracts.
An exact host-capacity test fills an unrejected report with serving identity and
cost metadata, then separately fills a failure report and spends released space
before terminal application. A retired attempt likewise admits a full canonical
result and a full report after competing state exhausts admission; one extra
metadata byte is rejected without changing its durable head.

The legacy-takeover fixture now retains a fixed 16 KiB cleanup margin rather than
half the canonical allowance: version 3's smaller canonical allowance is no
longer a useful legacy-cleanup estimate. The same padded state still admits the
unversioned contract and rejects takeover with the new reservation. No frozen
output limits or production policies were relaxed for this fixture.

Two surgical negative checks kept all version-3 frozen limits unchanged, then
separately removed the pre-receipt report reservation or the post-receipt error
reservation. Both exact-boundary cases failed with checkpoint cleanup capacity
exhausted; production source was restored after each check. Final independent review of the implementation, tests and OSS documentation
found no remaining actionable P1/P2 in this increment; it does not establish
full-goal acceptance. Previous head `97c77cf7` passed CI 37185091360, including platform jobs.


Final workspace/all-feature nextest passed **4038 tests, 22 skipped** in
207.446 seconds with four test threads (359.514 seconds including compilation).
Strict workspace/all-target/all-feature clippy passed in 49.922 seconds, strict
rustdoc in 28.711 seconds, workspace doctests passed five tests with one ignored,
and Rust 1.93.0 workspace/all-feature check passed in 23.855 seconds. Formatting,
diff and tracked-ignore checks passed. New-head remote CI remains a separate
gate; the full C4–C6/A01–A23 acceptance contract remains incomplete.


## C4 auxiliary outcome admission

New model steps freeze `auxiliary_output_version: 1`. Before preparation,
input-count, rebuilt-context validation or provider-integration work starts,
core reserves its complete outcome in state and event copies. Each report has
an allowance of 4096 bytes plus its serde-JSON request identity. Validation also
reserves the known candidate history it may activate. Legacy steps without the
marker retain their existing admission behavior; unknown versions or journal
policy changes are rejected before ownership advances.

Controlled SDK categories bound preparation, validation and integration error
reports. Arbitrary count metadata above the allowance becomes an explicit
`report_rejection` with a versioned byte-count/SHA-256 commitment and the original
numeric count, including zero. The numeric count is retained as evidence;
missing request binding means it cannot prove fit. A rejected count stops later
counts and generation. The commitment covers serde JSON before projection and
does not retain readable raw counter metadata.

An unsettled step also reserves failure delivery. Reasons through 1024 serialized
bytes remain readable; larger diagnostics retain a byte-count/SHA-256 commitment
and a controlled terminal reason. Completed preparation failures, context
denials and rejected counts remain terminal through outcome ACK loss and process
replacement, with cancellation and accepted steering retaining their precedence.
An independent review found that `HookDecision::Deny` was being recorded as a
successful callback before conversion to an error. A new regression reproduced
cold recovery continuing that request. All three preparation hook groups now
classify Deny and checked-selector violations inside the observed operation.

These are durable logical-capacity contracts. Trusted extension allocation,
new tool intents, later prompts, repeated archive growth, physical storage and
remaining C4–C6/A01–A23 conformance still require their own evidence. Targeted
checks, negative reservation checks, final workspace gates and review follow.

Forty-six targeted tests passed in 48.762 seconds. Coverage includes all three
Deny hook groups, pre/post-persistence outcome ACK loss, process replacement
before failure application, exact count limits and insufficient envelope
rejection before callbacks. Recovery rejects forged markers, oversized reports,
count commitments, altered request identities and count summaries forged into
fit evidence without advancing the durable head. A held callback remains
recordable after competing state saturates checkpoint admission, including
exact batch retransmission, bounded failure cleanup and release.

At an exact host boundary, each of the four complete auxiliary outcomes still
fits. The test then spends newly released capacity before maximal readable or
oversized failure delivery. Two negative checks independently disabled only
the report reservation or failure reservation, preserving every frozen limit;
both reproduced `checkpoint cleanup capacity exhausted`. Production source was
restored after each experiment. Full workspace validation follows. Previous
head `c4e9168f` passed CI 37188213974.

Final Rust 1.99.0 workspace/all-feature nextest passed **4046 tests, 22 skipped**
in 215.827 seconds with four test threads (371.088 seconds including compilation).
Strict workspace/all-target/all-feature clippy passed in 31.875 seconds, strict
rustdoc in 54.129 seconds, workspace doctests passed five tests with one ignored,
and Rust 1.93.0 workspace/all-feature check passed in 45.346 seconds. Formatting,
diff and tracked-ignore checks passed. Final independent source/test/documentation
review found no remaining actionable P1/P2 in this increment and confirmed both
reservations were restored after the negative checks. New-head remote CI remains
a separate gate; complete C4–C6/A01–A23 acceptance remains open.


## C4/C5 shipped-process crash conformance

The managed API fixture now starts the shipped `bro serve` executable, with its
own temporary configuration and virtual-key database. An independent HTTP and
WebSocket client retains checkpoint bytes, harness-issued ownership grants and
tool results on disk. It kills the core process without graceful shutdown,
starts a new instance, validates the retained journal against its grants and
restores under a higher epoch. No live core registry or session object crosses
the process boundary.

The checkpoint matrix stops at model intent, complete model outcome, applied
model output, completed response, accepted tool result and terminal run. Each
barrier is tested before persistence and after persistence with its ACK withheld.
The harness executes an actual append-and-sync file write. Assertions cover the
single effect, immutable response replay, preserved run identity, provider call
counts, stale HTTP epoch rejection and settled release. A lost complete outcome
can require another provider attempt; the original uncertain intent remains in
the cost inventory with unknown cost, never an invented zero estimate.

Two additional cases cover a delivered but unstarted command and an unknown
write effect. The former preserves invocation/attempt identities while fencing
the command to the replacement epoch. The latter keeps execution in
`recovery_required` even after a continuation records a definite live result.
A subsequent authenticated restoration confirms the independently retained
result before execution resumes; original uncertainty remains in the journal.

These tests exercise real process death and the production CLI/API/controller
path, with a loopback simulated provider and a deterministic test harness. Their
activity handoffs are quiescent: the provider or tool has stopped, or dispatch
has not begun. They do not demonstrate power-loss durability, remote Running
tool clock handoff, real-provider reconciliation, production harness integration
or the complete C4–C6/A01–A23 acceptance contract. Validation and independent
review results follow. Previous head `73560289` passed CI 37191002776 on all
configured platforms.

Three focused tests covering fourteen scenarios passed in 43.472 seconds. The
unknown-effect test also retains a definite live result without clearing the
recovery barrier, then resumes after a second process replacement confirms that
result. Its original uncertainty observation remains present. Independent
read-only review of the tests, helpers and documentation found no actionable
P1/P2. Full workspace validation follows; cross-platform execution remains a
separate remote CI gate.

Final workspace/all-feature nextest passed **4049 tests, 22 skipped** in
379.766 seconds with four test threads (391.049 seconds including compilation).
Nextest marked one unchanged extension-host test as leaky; its isolated rerun
passed in 0.205 seconds without that marker. The new process tests left no CLI
child running. Strict workspace/all-target/all-feature clippy passed in 4.863
seconds, strict rustdoc in 14.227 seconds, workspace doctests passed five tests
with one ignored, and Rust 1.93.0 workspace/all-feature check passed in 0.772
seconds. Formatting, diff and tracked-ignore checks passed. New-head remote CI
and complete C4–C6/A01–A23 acceptance remain separate gates.


## C4/C5 default tree and outstanding-tool limits

The default-count fixture creates a depth-four tree and holds four real executor
futures concurrently under unchanged `Limits::default()`. It rejects a fifth
child depth and, in the 32-agent cases, a thirty-third agent. Replaying either
rejected operation preserves its receipt and durable head. Agent-specific initial
answers and per-agent prompt/step counts verify attribution across the tree.
Parents receive each child's exact turn identity, answer and context sources once.

Tool work starts after the other branches have completed and the root has
consumed their conclusions. This explicit barrier separates outstanding-tool
contracts from concurrent join-model reports. A six-agent case returns eight
replies at each invocation's complete frozen JSON payload limit; a 32-agent case
completes with one such reply. Each retained result must pair exactly once with
its provider call ID, tool name and output. Terminal ACK loss reconnects without
additional executor calls or tool dispatch, and the SDK settlement count matches
the executed-call count. Every retained batch stays inside the default checkpoint and wire
bounds, followed by settled release.

The pressure variants first admit eight tools in the 32-agent tree, then submit
full-size optional tool observations until actual byte admission fails. The
required full results must still fit through failure cleanup and terminal ACK
loss both before and after persistence. Separately, a six-agent tree advertising
only 4 MiB of artifact capacity rejects the batch before dispatch. The adequate
fixture advertises 256 MiB; it never increases the core's 8 MiB checkpoint,
16 MiB wire or 64 KiB input limits. Count ceilings and byte ceilings apply
together, without promising that every maximum can coexist with arbitrary
retained history.

These are in-process scheduler/checkpoint tests with simulated model and harness
work. The complete JSON replies contain escape-heavy workspace revision metadata,
while referenced artifact bodies are five-byte fixtures. The tests do not
measure full artifact-body storage, cold archive restoration, physical memory or
production harness behavior. Validation and independent review results follow;
complete C4–C6/A01–A23 acceptance remains open.

Final workspace/all-feature nextest passed **4054 tests, 22 skipped** in
298.097 seconds with four test threads (298.867 seconds including compilation).
All five new cases passed, including actual saturation with terminal ACK loss
before and after persistence. Strict workspace/all-target/all-feature clippy
passed in 7.388 seconds, strict rustdoc in 3.495 seconds, workspace doctests
passed five tests with one ignored, and Rust 1.93.0 workspace/all-feature check
passed in 0.606 seconds. Formatting, diff and tracked-ignore checks passed.
Independent source/test/documentation review found no remaining actionable
P1/P2 in this increment. Previous head `923b5494` passed CI 37193107781; new-head
remote CI and complete C4–C6/A01–A23 acceptance remain separate gates.


## C4 restoration decoding lifetime

Restoration now counts its complete control envelope without allocating a second
serialized request. It authenticates every original batch, ownership transition
and the final durable head first, dropping each decoded payload as it proceeds.
Only after that complete pass does it decode and hydrate one snapshot at a time
for the existing release, resource, response, wait-policy, auxiliary-policy and
activity-history validators. Only the final hydrated payload is retained. A bad
late batch cannot cause archive reads through an otherwise valid prefix.

Archive hydration consumes the compact checkpoint JSON and releases the raw
archive bytes after owned deserialization, before rebuilding hydrated JSON.
Ordinary snapshots retain their original JSON representation: inserting omitted
optional fields during normalization can break legitimate legacy receipt/event
equality. A legacy release regression covers omitted `error` fields in both
retained copies. Other tests produce real archives through successive restores,
reject corrupt late chain metadata and oversized envelopes before any artifact
read or ownership registration, reject an intermediate policy rewrite, and
preserve complete tool observations and cumulative activity through restoration,
settlement and release.

This removes avoidable request/journal/archive buffer overlap. It does not claim
a measured RSS ceiling or complete physical memory accounting. The encoded
request, current typed/JSON representations and historical validation facts
remain live as needed. Physical storage leases, historical checkpoint retention
and the remaining C4–C6/A01–A23 requirements are still open.

CI 37204394325 for prior head `bdbdf1a1` exposed a Linux fixture wait timeout
before the tool-bearing model was released; two other large cases passed close
to the ordinary five-minute cutoff. The tree fixture now selects the earliest
root leaf by scheduler order, labels each timed barrier, and runs the three
large checkpoint-history cases in one nextest group. Each still exercises four
overlapping model futures with the same product limits, assertions and overall
test timeout. Validation and independent review results follow.

Final workspace/all-feature nextest passed **4057 tests, 22 skipped** in
500.440 seconds with four test threads (596.623 seconds including compilation).
All three restoration regressions passed. The three large tree cases passed
in 141.400, 128.865 and 139.412 seconds with their unchanged timeout. Strict
workspace/all-target/all-feature clippy passed in 33.009 seconds, strict rustdoc
in 20.955 seconds, workspace doctests passed five tests with one ignored, and
Rust 1.93.0 workspace/all-feature check passed in 21.100 seconds. Formatting,
diff and tracked-ignore checks passed. Independent source/test/documentation
review found no remaining actionable P1/P2 in this increment and confirmed
that fixture changes preserve coverage and product limits. Prior-head CI
37204394325 completed with the Linux workspace failure described above; its
other platform jobs passed. New-head CI and complete C4–C6/A01–A23 acceptance
remain separate gates.


## C5 local/remote collaboration parity

A shared provider script now drives the assembled application through both
`CoreSession` and independent authenticated HTTP/WebSocket clients. It emits
an explicit inherited-context spawn, an isolated fresh-context delegation and
a wait. Two child provider requests must both reach an asynchronous barrier
before either can finish, under a two-model run limit. This proves actual
request overlap without depending on a response delay or a synthetic counter.

Each path checks selected context against the actual provider input, exact
child/turn/parent ownership, one child execution and one consumed conclusion,
and decision/application/allocation/attempt identity joins. Applied decisions
and actual provider/model receipts remain attributable. Collaboration calls
never become workspace dispatches. The normalized allocation, routing and
answer evidence must agree between the two ports.

The remote client additionally checks each child's exact JSON and SSE text and
agent path, child-delivery event identities, the root's final-answer ownership,
terminal run status and empty pending-tool map. Replaying the accepted input as
JSON or SSE returns the same completed exchange without additional provider
requests. Wait target identities remain exact, while intermediate wait status
and root scheduling may vary: `wait_agent` is not a join-all operation.

This supplies a concrete cross-transport scenario for A01/A04 and context/output
attribution. It does not establish all collaboration/reuse/cancellation cases,
credentialed real-provider behavior, production harness conformance or complete
C4–C6/A01–A23 acceptance. Validation and independent review results follow.

The final targeted parity case passed in 12.961 seconds. Final
workspace/all-feature nextest passed **4058 tests, 22 skipped** in 514.068
seconds with four test threads (520.174 seconds including compilation); the
new case passed in 14.484 seconds under suite load. Strict
workspace/all-target/all-feature clippy passed in 2.274 seconds, strict rustdoc
in 8.276 seconds, workspace doctests passed five tests with one ignored, and
Rust 1.93.0 workspace/all-feature check passed in 0.526 seconds. Formatting,
diff and tracked-ignore checks passed. Independent source/test/documentation
review found no remaining actionable P1/P2 in this increment. New-head remote
CI and full C4–C6/A01–A23 acceptance remain separate gates.


## C4 checkpoint wire admission cost

CI 37208007310 for `0655f64c` failed the Linux workspace shard at the
32-agent fixture's pre-tool join deadline. Its last snapshot still recorded
recent activity; independent source review found no mandatory lock cycle.
A local serial baseline passed all three large cases in 108.330, 104.484 and
111.003 seconds. CPU sampling showed substantial time repeatedly counting the
large base64 string as JSON during checkpoint wire-limit checks. This evidence
supports a CPU-cost diagnosis, not a proven permanent scheduler deadlock.

Checkpoint encoding and decoding now count the actual empty-payload envelope
and add the base64 byte length with checked arithmetic. This is exact for
valid base64, which never requires JSON escaping. Decode first rejects an
oversized lower bound, then strictly decodes the payload; malformed base64
falls back to exact JSON wire counting before returning its decode error.
Thus escaped malformed payloads retain the previous size-error precedence.
The public `wire_bytes` method continues to count arbitrary inputs exactly.
No schema, hash, ownership, ACK, negotiated limit or admission policy changes.

Three regressions compare admission with the serialized `ServerMessage` at
its exact size and one byte below, cover base64 padding residues and a large
payload, reject malformed base64 while preserving error precedence, and count
escaped untrusted identity/digest fields before rejecting their integrity.
The 32-agent fixture also records head revisions and actual executor counts
while awaiting joins so future timeouts distinguish progress from a stall.
Its scale, four overlapping model calls, assertions and deadlines are unchanged.

The complete 24-test checkpoint contract suite passed. Large-fixture comparison,
workspace validation and final independent review are recorded below when done.
Previous head `e1da9e15` passed every job of CI 37206392220. The later Linux
failure is not treated as a successful CI run; new-head CI remains a separate
gate. Full C4–C6/A01–A23 acceptance remains open.


The same three local serial cases now pass in 75.630, 74.670 and 77.411 seconds
(227.712 seconds combined versus the 323.819-second baseline). Random agent
identities and join scheduling remain variable; these are observed local
measurements, not a universal speedup or a Linux CI pass. Progress diagnostics
show increasing durable revisions during each wait. Independent code, regression
and documentation review found no actionable P1/P2. Full workspace gates are
running sequentially before submission.


Final Rust 1.99.0 workspace/all-feature nextest passed **4061 tests, 22 skipped**
in 395.623 seconds with four test threads (489.136 seconds including compilation).
The three large cases passed under suite load in 112.280, 98.752 and 101.760
seconds. Strict workspace/all-target/all-feature clippy passed in 32.795 seconds,
strict rustdoc in 19.572 seconds, workspace doctests passed five tests with one
ignored, and Rust 1.93.0 workspace/all-feature check passed in 19.130 seconds.
Formatting, diff and tracked-ignore checks passed. No failed, timed-out or leaky
test was reported. Independent source/test/documentation review found no
remaining actionable P1/P2 in this increment. New-head remote CI and the full
acceptance contract remain separate verification gates.

## C4/C5 process death during an incomplete provider response

Two additional shipped-CLI process cases hold a nonstreaming provider HTTP
response open while killing `bro serve`. An independent raw HTTP fixture first
answers input counting, then sends either a partial JSON tool argument or a
complete-looking JSON response whose declared HTTP body still has missing
bytes. It reports when the prefix has been written and observes the old
connection close after the process is killed. These are HTTP entity/body cases,
not SSE streaming coverage or proof of exactly which bytes the client decoded.

The replacement loads only the independent harness's saved journal and grants.
The original provider attempt has no complete receipt, tool invocation or
applied collaboration call. Its cost exposure retains its original request,
attempt and provider source, with unknown time and token cost. Restoration
without a stopped-owner attestation or without cumulative activity is rejected
without provider dispatch. A successful restoration closes the interrupted
step; its checkpoint must be acknowledged before new work is admitted. Retrying
the pending response keeps the run identity but creates a new step, SDK request
and provider attempt. Only the fresh complete response authorizes a file write.
Completion retains the original uncertain attempt, two new outcomes, one file
effect, fenced old epochs and immutable response replay without extra calls.

The positive restoration path deliberately supplies a **scripted cumulative
activity attestation** as trusted fixture input. It is not measured elapsed
activity and is not derived from network arrival, RTT or restart downtime.
Consequently these tests exercise crash recovery conditional on trusted activity
input; they do not establish a production clock handoff, remote Running-tool
restoration, real-provider reconciliation or full C4–C6/A01–A23 acceptance.
Production timing and harness integration remain separate requirements.

The five process tests passed all sixteen scenarios in 21.179 seconds with two
test threads. Independent source/test/documentation review found no actionable
P1/P2. Its optional suggestion strengthened the failed-restoration HTTP probe to
use the new epoch and require `unauthorized_scope`, specifically checking that
no usable binding was installed. Final workspace gates follow. Prior head
`33e293cd` has passed all Linux CI jobs; macOS and Windows tests remain pending
at this snapshot. This is not an overall remote CI success claim.


Final Rust 1.99.0 workspace/all-feature nextest passed **4063 tests, 22 skipped**
in 401.049 seconds with four test threads (407.557 seconds including compilation).
Both new cases passed under full-suite load, including the strengthened
new-epoch rejection probe. No failed, timed-out or leaky test was reported.
Strict workspace/all-target/all-feature clippy passed in 2.447 seconds, strict
rustdoc in 9.569 seconds, workspace doctests passed five tests with one ignored,
and Rust 1.93.0 workspace/all-feature check passed in 0.555 seconds. Formatting,
diff and tracked-ignore checks passed. The production protocol, limits, clocks
and core behavior are unchanged; this increment adds conditional fault evidence.
New-head CI and full acceptance remain separate gates.


## C4 pending failure status during new tool-batch admission

A child can finish its model request but exceed the artifact quota when core
admits its new tool reply and recovery-archive obligations. If the independent
capacity-failure checkpoint loses its ACK, another agent can encounter a
blocked dispatch before the scheduler receives the original job error. The
aggregate driver previously returned that later `checkpoint_unavailable /
not_committed` rejection, hiding the still-unreconciled submitted batch.
Re-entering the driver reproduced the same status loss deterministically.

The shared scheduler now preserves `unknown` when a `checkpoint_unavailable /
not_committed` error coincides with a pending checkpoint. Both ordinary driving
and active Responses scheduling use this boundary. Other errors and a
disconnected driver without a pending batch retain their previous status.
Named operation APIs retain their own acceptance semantics: a rejected caller
operation can still be `limit_exceeded / not_committed` after a separate capacity
failure commits. Completed-exchange replay paths are outside this scheduler
normalization; this is not a blanket change to every response error.

The new admission matrix retains an acknowledged complete provider outcome
before rejecting a tool batch, loses the capacity or terminal ACK before/after
persistence, and reconciles exactly the same batch once. One variant has no
existing tools; another has a root tool reported Running and another awaiting
approval while a child requests four new tools. Six calls fit the default count
of eight. The same workload succeeds with sufficient artifact quota and
unchanged core limits. Failed admission preserves the original model receipt,
usage, cost identity and settlement count without dispatching the new batch.
It fences the unstarted tool, sends cancellation, retains full-size serialized
Stopped/result payloads for the original tools, pairs results once, preserves
input replay and completes failed-run cleanup and ownership release.

Separate regressions cover no-pending disconnect status and active Responses
scheduling/replay. Cancellation delivery is observed through a harness barrier,
not assumed synchronous with the driver. Referenced artifact bodies remain
five-byte fixtures; full serialized payloads do not prove maximum artifact-body
capacity, physical storage reservation or production tool execution. Later
prompt overflow, repeated recovery/archive growth, storage-full faults, measured
remote Running handoff and production harness acceptance remain open. Prior
head `a841e3c1` passed every job of CI 37212314200 on Linux, macOS and Windows.

Focused validation passed five tests covering twelve scenarios in 3.233 seconds
(15.462 seconds including compilation). Independent source/test review found
no actionable P1/P2. Final Rust 1.99.0 workspace/all-feature nextest passed
4068 tests with 22 skipped in 436.462 seconds (617.863 seconds including
compilation, four test threads), with no failed, timed-out or leaky tests.
Strict workspace/all-target/all-feature clippy passed in 101.402 seconds,
strict rustdoc in 32.676 seconds, and workspace doctests passed five tests with
one ignored. Rust 1.93.0 workspace/all-feature check passed in 103.475 seconds;
formatting, diff and tracked-ignore checks passed. This increment changes the
aggregate scheduler's error status and adds admission fault evidence, while
retaining existing protocol fields, limits and clocks. New-head CI and the
remaining full acceptance audit are separate gates.


## Integrating the updated native runtime base

The scheduler-status fix was committed and pushed as `965a1c02`. GitHub then
reported the stacked PR as conflicting with its updated native runtime base,
`9981d7a2`. The base now includes durable runtime recovery, a unified Thread/Turn
execution model, ownership and history indexes, and the six-tool integration.
Its rewritten ancestry shares an older merge base with this branch, so the merge
also reports conflicts in code that has no core-specific changes.

Resolution compares core changes against its original `a58f40ca` base. Files
without core changes adopt the updated native implementation. Shared integration
files retain ManagedCoreApi startup, inference-listener routing and shutdown,
the public core module, SDK model constraints, dependencies and managed-core
skill/documentation entry points. The native runtime uses ThreadService and the
updated CLI, storage migrations and plugin descriptions from its own base.

This is dependency integration; it does not establish that the native runtime
already delegates scheduling to managed core. Production harness integration,
measured remote Running handoff, remaining capacity fault evidence and the full
acceptance audit remain required. The earlier 4068-test result applies to
`965a1c02`; validation and independent review of the merged tree are recorded
separately after completion.

Independent merge review found no actionable P1/P2 and confirmed preservation
of every core change against its original base. The merged Rust 1.99.0
workspace/all-feature nextest run passed 4165 tests with 22 skipped in 414.625
seconds (524.835 seconds including compilation, four test threads). There were
no failed or timed-out tests; nextest marked the unchanged SDK
`observe::schema::tests::the_declaration_renders` test as leaky. An isolated
all-feature SDK schema run passed all six tests in 0.030 seconds without a leak
report (40.569 seconds including compilation). The initial report remains
recorded; its cause was not reproduced or established by that rerun.

Strict workspace/all-target/all-feature clippy passed in 38.279 seconds,
strict rustdoc in 22.226 seconds, and workspace doctests passed five tests with
one ignored. Rust 1.93.0 workspace/all-feature check passed in 22.462 seconds;
formatting, diff and tracked-ignore checks passed. New merged-head CI and the
remaining acceptance work are separate gates.


## Reusing verified checkpoint payloads

Merged head `512ef22d` failed Linux workspace CI 37317699745 in
`maximum_tree_byte_failure_survives_terminal_ack_loss_after_persistence`.
The 32-agent test continued advancing durable revisions but reached its
240-second join deadline. A local direct-binary baseline passed in 80.525
seconds. CPU sampling showed repeated full checkpoint JSON decoding in both
core proposal validation and the durable harness fixture.

`CheckpointBatch::validate_append_with_payload` now returns the admitted ACK
and decoded payload together. Harnesses can use the same verified payload for
artifact checks, tool-start fences and event persistence. Existing
`validate_append` remains available with its original return type. Neither
method performs persistence: validation, required artifact byte checks and
atomic append/fences still belong in the harness transaction, and the ACK may
be sent only after durability. Digest, schema, scope, epoch, base ordering,
artifact-reference and exact-replay checks remain in place.

Core proposal validation, untrusted harness admission and ACK consumption
still decode the hashed bytes. Independent review identified that bypassing
proposal decoding could admit JSON beyond the decoder's nesting-depth limit;
that shortcut was removed and a regression now verifies rejection before any
pending proposal is retained. The durable fixture reuses one validated payload,
and its outer fault wrapper only decodes when an event-dependent fault is
configured. No extra parsed checkpoint is retained in the gate. Test tree size,
overlapping model futures, tool counts, product limits and deadlines remain
unchanged.

Contract regressions check foreign scope/epoch rejection without retaining a
proposal, unchanged returned state/fences, historical ACK replay without head
rewind, and corrupted bytes despite a retained ACK. Later model-step admission
experiments are kept outside the compiled suite until their fault assertions
are complete; they are not counted as acceptance evidence. Performance,
independent review and full validation results are recorded below when complete.


After restoring proposal decoding, focused validation passed all 27 contract
tests and the previously failing large-tree case: 28 tests in 63.266 seconds
(80.316 seconds including compilation). The large-tree case took 63.235 seconds,
compared with the earlier local direct-binary baseline of 80.525 seconds.
Random agent identities and scheduling vary; this is a local observation, not
a cross-platform speed guarantee or a Linux CI pass. Follow-up independent
review found no remaining actionable P1/P2. Full workspace gates follow.


Final Rust 1.99.0 workspace/all-feature nextest passed 4168 tests with 22 skipped
in 317.880 seconds (390.438 seconds including compilation, four test threads).
The three large-tree cases took 84.225, 74.802 and 75.311 seconds. No tests
failed or timed out. Nextest marked the unchanged SDK
`observe::schema::tests::the_committed_artifact_matches_the_declaration` as
leaky. The isolated six-test schema group then passed in 0.016 seconds without
a leak report (0.614 seconds including build checks), using the same workspace
feature selection. The original leak report remains recorded; its cause is
not established by that rerun.

Strict workspace/all-target/all-feature clippy passed in 31.286 seconds,
strict rustdoc in 21.205 seconds, and workspace doctests passed five tests with
one ignored. Rust 1.93.0 workspace/all-feature check passed in 21.714 seconds;
formatting, diff and tracked-ignore checks passed. Independent source/test/
documentation follow-up found no remaining actionable P1/P2. This increment
changes a Rust admission helper and removes redundant fixture decoding; it
does not change wire fields, limits, clocks or persistence ownership. New-head
CI and the remaining C4–C6/A01–A23 acceptance work remain separate gates.


## Cancellation start fences and later model admission

The later-model capacity control exposed an unstarted-tool race in ordinary
cancellation. Run/subtree cancellation and root failure recorded cancellation
intent but omitted tool-start fences from the same checkpoint. A delayed
approval could therefore start after the cancellation append but before the
asynchronous `tool.cancel` message. Steering and resource failures already had
this durable barrier.

Checkpoint preparation now adds fences for newly cancelled turns, comparing
both run and turn identity rather than individual event names. This covers run
cancellation, runtime/model subtree interruption and descendants cancelled by
root failure. Restoration reasserts fences for retained cancellation, including
historical unfenced cancellation checkpoints. Unresolved `EffectUnknown`
results remain included without being changed or consumed. Steering, resource
and cancellation fences are deduplicated before encoding. Existing cleanup
projection already reserves every unfinished invocation's fence; no limit,
wire field or persistence ownership changes.

Eight fault scenarios lose the run, runtime-interrupt, model-interrupt or
root-failure ACK before/after persistence. They inspect durable harness fences
before any cancel delivery, exercise start-before-cancel and cancel-before-start,
retain real started-work obligations, and verify child/grandchild scope with
unaffected parent/sibling admission. Stale cancellation creates no fences;
reconnect adopts or replays the exact batch once. Two legacy recovery cases
reassert fences for pending approvals or an unknown committed result. The
unknown case stays `RecoveryRequired`; direct replacement of its result is
rejected, and a second authenticated restore confirms the actual outcome while
retaining the original uncertainty. Recovery timing is deterministic trusted
fixture input, not a production remote-clock measurement.

The later-model matrix accepts three serialized maximum-size text replies into
history. The fourth step commits its prepared history and plan, then fails
attempt admission without entering the provider executor. Failure and terminal
ACK loss before/after persistence are covered for a root and for a child while
its parent owns a Running and a WaitingApproval tool. Earlier operation receipts,
call/result pairing, provider receipts, usage and cost identities survive.
The SDK settles the rejected request once; that callback is distinct from a
provider attempt and is not repeated on replay. Full-size serialized old-tool
Stopped/result payloads remain admissible through cleanup and release; their
referenced bodies are five-byte fixtures. Positive controls preserve the same
history and other limits, increase checkpoint/wire capacity, and verify the
provider receives the retained replies.

These five new tests cover twenty scenarios. They do not replace physical
storage reservation, broader recovery/transport fault conformance, measured
remote Running handoff, real-provider accounting or production-harness
acceptance. Prior head `090decfe` passed all jobs of CI 37322792865, including
Linux, macOS and Windows. Final validation and independent review of this
increment are recorded below when complete.


Final source/test/documentation review found no remaining actionable P1/P2.
The fixture now records starts before returning successful tool results,
including the model-driven cancellation actor. Strict clippy and formatting
passed. The final focused six-test group passed in 12.046 seconds (22.962
seconds including compilation), including the five new tests and the existing
live-provider interruption regression. Nextest marked the sufficient-capacity
control as leaky in this run; its original report remains retained, and the
full-suite result and isolated recheck are recorded below when complete.


Final Rust 1.99.0 workspace/all-feature nextest passed 4173 tests with 22 skipped
in 336.224 seconds (407.839 seconds including compilation, four test threads).
There were no failed, timed-out or leaky tests in that full run. All five new
tests passed under the full load; the three large-tree cases took 85.025,
77.648 and 79.237 seconds. The sufficient-capacity control also passed its
isolated workspace-feature rerun in 2.048 seconds (2.659 seconds including
build checks), without a leak report. The earlier focused-run leak report
remains recorded; these reruns do not establish its cause.

Strict workspace/all-target/all-feature clippy passed in 6.547 seconds,
strict rustdoc in 21.504 seconds, and workspace doctests passed five tests with
one ignored. Rust 1.93.0 workspace/all-feature check passed in 22.001 seconds;
formatting, diff and tracked-ignore checks passed. Independent source/test/
documentation review found no remaining actionable P1/P2. New-head CI,
remaining cross-transport/recovery pressure cases and full C4–C6/A01–A23
acceptance still require their own evidence.


## Independent uncertain-observation reserves and repeated archive pressure

Independent review found that both artifact-body and checkpoint cleanup
accounting treated an `EffectUnknown` outcome as having consumed the first
`EffectUnknown` lifecycle observation. The API permits either order, and the
frozen reply contract reserves these as separate messages. Two regression
cases reproduced rejection of that first full observation after an uncertain
result, a first stopped report and optional repeated stopped reports exhausted
capacity: one failed on artifact quota, the other on checkpoint cleanup bytes.

Both counters now determine unknown-observation consumption from actual
observations only. The running-phase restriction after a result, separate
outcome slots, definite-outcome release, frozen reply bounds and wire fields
remain unchanged. The tests retain maximum serialized payloads and actual
maximum artifact bodies for uncertain/definite results and essential reports.
Four scenarios cover artifact/checkpoint pressure with the first unknown
observation ACK lost before/after persistence. Exact retransmission/adoption,
operation replay, accepted optional evidence, prior uncertainty, provider cost
identity, failed cleanup and ownership release are checked without new model
or workspace execution.

A separate test repeatedly restores two started tools with maximum serialized
Running observations and zero-byte evidence bodies until the growing archive
JSON reaches logical quota refusal.
It preserves every accepted observation and exact cumulative activity record;
the refused restore does not advance the durable head. First stopped and
unknown observations still commit with maximum bodies and serialized payloads,
including restore ACK loss before/after persistence. Full uncertain results
remain blocked until authenticated definite restoration; original uncertainty,
input receipts, provider outcomes and histories survive through cancellation
and release. The initial run accepted nine Running handoffs before refusal;
the test checks the boundary rather than freezing that incidental count.
Activity evidence is deterministic trusted fixture input, not measured remote
handoff. Historical roots remain pinned in the fixture, so these tests do not
establish physical storage leases or reclamation.

This increment does not complete storage-full staging/commit faults, other
recovery and transport pressure combinations, production harness integration,
measured remote Running handoff or real-provider billing reconciliation. Full
C4–C6/A01–A23 acceptance remains open. Final validation and independent review
are recorded below when complete.


Final Rust 1.99.0 workspace/all-feature nextest passed 4176 tests with 22 skipped
in 337.113 seconds (337.647 seconds including build checks, four test threads).
No failed, timed-out or leaky tests were reported. The three new tests passed
under full load; the large-tree cases took 85.857, 77.522 and 80.325 seconds.
The related artifact/tool-payload/restore group passed 24 tests in 3.030 seconds
(3.790 seconds including build checks). Original red regressions are retained:
both artifact and checkpoint saturation rejected essential unknown observations
before the counter repair; the final three-test focused run passed in 1.407
seconds.

Strict workspace/all-target/all-feature clippy passed in 30.929 seconds,
strict rustdoc in 20.614 seconds, workspace doctests passed five tests with one
ignored, and Rust 1.93.0 workspace/all-feature check passed in 21.840 seconds.
Formatting, diff and tracked-ignore checks passed. Independent source/test/
documentation review found no remaining actionable P1/P2. Legacy reply limits
are preserved; historical snapshots already lacking required cleanup capacity
can still be refused by conservative restoration admission. Parent head
`8c880345` passed every job of CI 37341576194, including Linux, macOS and Windows.
New-head CI and complete C4–C6/A01–A23 acceptance remain separate gates.


## Archive storage faults and explicit definite-result confirmation

Five deterministic fault scenarios exercise archive staging with zero bytes
available, staging limited to its first 8192-byte chunk, completion without a
successful send return, atomic checkpoint rejection after validation but before
append, and an append whose ACK is lost. They use core-generated multi-chunk
archives containing accepted recovery history and references to maximum-body
stopped evidence. The checkpoint/store retain cancellation fences. Pre-append
failures leave the durable head unchanged; a lost append ACK leaves only the
local head behind the advanced durable head. Dependent execution remains blocked.
Partial staging cannot be read
as an artifact; complete roots and all dependencies are verified before append.

Same-owner recovery resends identical archive references and chunk bytes.
Before-append faults directly compare all retransmitted checkpoint bytes; an
already appended batch is adopted once. The original root remains available,
accepted operation/model/cost records and exact activity/tool histories survive,
and no model or workspace tool is reexecuted. Existing cancellation tombstones
are preserved; this fixture does not independently exercise insertion of new
fences during a failed append. Full definite results, explicit subsequent
restoration, cancellation and release complete cleanup.

This path exposed a recovery trap: a live definite result received after
missing or stopped evidence could be durably retained, yet repeating that same
result in an authenticated restore never cleared `RecoveryRequired`. The
confirmation predicate only recognized historical `EffectUnknown` observations;
neither a first-result nor a changed-result branch applied to an identical
retained outcome. The predicate now treats every validated explicitly supplied
definite result as confirmation. Identity, conflict, payload and artifact
checks precede it; aggregate unresolved-tool checks still dominate. The live
receipt path and empty restore behavior are unchanged.

Four focused cases cover missing/stopped evidence with ordinary execution or
cancellation. Live results and an empty restore remain blocked; explicit
identical confirmation completes the run or its cancellation without rerunning
the tool. Two sibling cases keep missing/unknown outcomes blocked even beside a
confirmed result, preserve prior uncertainty and finish only after full definite
reconciliation. The isolated red regression and archive-cleanup failure logs
retain proof of the original trap.

Four new tests cover eleven scenarios. Their handoff timing is deterministic
trusted fixture input; staging capacity and atomic failures are injected at the
harness boundary. They do not prove OS ENOSPC, physical storage leases,
historical-root reclamation, measured remote Running handoff or production
harness acceptance. Full C4–C6/A01–A23 acceptance remains open. Final validation
and independent review are recorded below when complete.


Final Rust 1.99.0 workspace/all-feature nextest passed 4180 tests with 22 skipped
in 333.480 seconds (334.037 seconds including build checks, four test threads).
No failed, timed-out or leaky tests were reported. The four new tests passed
under full load; the large-tree cases took 83.690, 75.823 and 78.158 seconds.
The related archive/recovery/reconnect group passed 50 tests in 20.363 seconds
(21.074 seconds including build checks), including the existing real-CLI
process-crash matrix. The four new tests passed their focused run in 3.085
seconds. Retained red logs show explicit identical confirmation failing before
the repair, both in isolated missing-evidence recovery and archived stopped
cleanup; earlier storage-test assertion corrections were not production bugs.

Strict workspace/all-target/all-feature clippy passed in 30.954 seconds,
strict rustdoc in 20.783 seconds, workspace doctests passed five tests with one
ignored, and Rust 1.93.0 workspace/all-feature check passed in 21.686 seconds.
Formatting, diff and tracked-ignore checks passed. Independent source/test
review found no remaining actionable P1/P2; documentation now distinguishes
archive references from checkpoint fences, durable from local heads after ACK
loss, and surviving-owner reconciliation from process-loss takeover. Parent
head `517997ae` passed every job of CI 37345155456, including Linux, macOS and
Windows. Final independent source/test/documentation/log and PR-draft audit
found no remaining actionable P1/P2. New-head CI and full C4–C6/A01–A23
acceptance remain separate requirements.
