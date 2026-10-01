# Orchestrator Core Implementation Record

This record tracks implementation of [specification v1.0](ORCHESTRATOR_CORE_SPEC.md).
The specification remains the acceptance contract. An interface, unit test, or
mock-provider demonstration does not establish production integration.

## Baseline and integration

- The specification was frozen against main `d93ed73`.
- Existing native BRO work is in PR #945; the six-tool and durable-record work
  is stacked in PR #951 (`a58f40ca`). Its execution loop predates the core/harness
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
| C3 | Context manifests and joint deterministic routing, hard feasibility and actual execution receipts | In progress: signals, prepared-plan records, worker allocation, output/input capacity, constrained reconstruction, production hook revalidation and token/cache evidence below; full protocol/continuation feasibility and complete accounting acceptance remain open |
| C4 | Crash restoration, epoch/head reconciliation, queue/steer/cancel and uncertain effects | Pending |
| C5 | Managed Responses and authenticated harness channel over the same core operations | Pending |
| C6 | Production harness, independent client, real-provider and pressure conformance | Pending |
| Delivery | Independent stage reviews, complete acceptance audit, all-feature tests/doctests/clippy/fmt, PR and CI | Pending |

Each stage receives an independent review. Findings and fixes are recorded with
the stage's actual validation commands. The final independent review checks the
whole contract rather than only the new diff.

## Acceptance evidence

All A01–A23 scenarios in the specification are **unproven** until executable
evidence is recorded here. Later stages must cover every scenario, including
remote/in-process parity, real concurrency, durable output and effect barriers,
crash/ACK-loss recovery, caller isolation, provider accounting, and actual
production-harness integration. No completed subset narrows the original scope.

## Validation environment

Local Rust: 1.95.0, aarch64-apple-darwin. Nextest is available. Builds use
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
of the run-cost contract. Preparation, input counting and integration work need
explicit cost categories, including durable unknown evidence when no amount is
available. Reported monetary charges and reconciled charges need independent,
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
