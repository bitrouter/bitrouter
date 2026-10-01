# BRO runtime v0.2 implementation evidence

Updated: 2026-10-01. Status: **R1 in progress; full MVP incomplete**.
Contract: [BRO runtime spec](BRO_AGENT_RUNTIME_SPEC.md), aligned with product 003
v0.2. Earlier [native implementation evidence](BRO_NATIVE_AGENT_IMPLEMENTATION.md)
belongs to the former runtime and does not prove the v0.2 acceptance gates.

## Phase ledger

| Phase | Current state | Remaining gate |
| --- | --- | --- |
| R0 | Concrete storage/commit and compatibility choices being implemented | Complete Thread/control/recovery DTOs, limits, ownership and operator resolution |
| R1 | Dedicated transactional records, commit acknowledgements and common call settlement implemented locally | App/DB integration, crash record evidence and repository checks |
| R2 | Not implemented | Actual bounded overlap, barriers, error/cancel cleanup |
| R3 | Not implemented | Continuous Thread context, durable queue/steer/approvals and keys |
| R4 | Not implemented | Load, ownership/effect investigation, safe checkpoint continuation |
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
- Local protocol v5 reflects new outcome semantics. No configured check reports
  not_requested. Continuous Thread/recovery commands are not published yet.

## Evidence so far

- `cargo check -p bitrouter-orchestrator -p bitrouter` passed before the additional
  fault tests and request-metadata changes; subsequent checks are still required.
- `cargo test -p bitrouter-orchestrator --lib`: 25 passed. Includes injected
  admission/response/intent/result commit failures, no second effect after lost
  result commit, cancelled-response call settlement, provider-ID reuse, original
  ordering, observer resync and subprocess cleanup.
- Database reopen/fault evidence and the required all-feature test/clippy/fmt
  gates are pending. No hosted CI, credentialed provider, Windows or real ACP
  client acceptance is claimed.

This record does not authorize automatic replay. R1 records are recovery inputs;
R4 must first establish old execution termination and effect status. Storage
failure may leave uncommitted cleanup information; last durable facts remain
the authority and the task stays blocked rather than claiming terminal success.

## Six base tools follow-up

The approved [six-tool contract](BRO_BASE_TOOLS_SPEC.md) is implemented locally.
Its [acceptance record](BRO_BASE_TOOLS_ACCEPTANCE.md) covers full source checks,
macOS real-model coding/read-only runs, and a controlled comparison with the
preceding seven-tool Unix interface. Windows declarations, output/exit/streaming, and descendant cleanup passed
hosted CI.
This tool slice does not complete R0–R6, continuous Threads, or restart recovery;
the phase ledger and earlier pending gates above retain their scope.
