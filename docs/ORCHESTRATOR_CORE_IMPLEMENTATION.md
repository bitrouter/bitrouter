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
| C1 | Root execution, shared prepared model pipeline, acknowledged step/output/tool/result barriers | Pending |
| C2 | Bounded concurrent child scheduling, durable collaboration, fair waits and cancellation | Pending |
| C3 | Context manifests and joint deterministic routing, hard feasibility and actual execution receipts | Pending |
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
