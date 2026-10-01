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
| C3 | Context manifests and joint deterministic routing, hard feasibility and actual execution receipts | In progress: reviewed signal/material groundwork below; joint routing pending |
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
