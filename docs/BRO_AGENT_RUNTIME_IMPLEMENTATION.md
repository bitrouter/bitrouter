# BRO runtime acceptance and evidence

Updated: 2026-10-06. Contract: [standalone runtime](BRO_AGENT_RUNTIME_SPEC.md).
This is the entry point for source-specific evidence. Each linked record identifies
its source and validation boundaries; earlier provider/platform evidence is not
reattributed to later changes.

## Latest recorded integration

[AGENTS.md working tree validation](BRO_HARNESS_RESOURCES.md#agentsmd-working-tree-validation-2026-10-06)
covers startup scope, durable user-context snapshots, nested reads and refresh
without known-call replay. Validation ran locally in a working tree based on `b9527673`.

[Native Harness resources](BRO_HARNESS_RESOURCES.md#mcp-and-skills-source-and-validation-2026-10-05)
retains the earlier local validation record for source `321ccaad`, based on PR #945's
`fb243ee5`. It covers production MCP execution, runtime-owned skills discovery,
format-2 reading/atomic format-3 append and the final workspace checks. Core
selection, skills activation and inbound ACP remain subsequent work.

[Conversation UI acceptance](BRO_CONVERSATION_UI_IMPLEMENTATION.md) retains the
earlier detailed validation record for source `485728f9`, including main `d8b66a9e`
and the six-tool integration. Documentation-only ledger head is `65555b12`.
It contains the final macOS/Rust 1.97.0 workspace results, commands, process/PTY
coverage and earlier leak/preflight/suspend fixture failures.

That run exercises actual client/service/SQLite/PTY paths with controlled upstream
fixtures. It does not establish fresh hosted CI, Windows/Linux UI, credentialed
providers, real billing, stress limits or production acceptance. Earlier warnings
and reruns remain in the linked record, not converted to successful first attempts.

## Regression responsibilities

| Contract | Executable evidence |
| --- | --- |
| AGENTS.md startup scope, user-context snapshots, nested reads and refresh without replay | `service/tests/instructions.rs`, `service/tests/harness.rs`, `apps/bitrouter/tests/native_harness_resources.rs` |
| MCP inventory, permissions, process cleanup and resource-bound continuation | `service/tests/harness.rs`, `apps/bitrouter/tests/native_harness_resources.rs`, `apps/bitrouter/tests/harness_mcp_check.rs` |
| Legal context, model responses, ordered tools, budgets and commit barriers | `agent.rs` tests; `service.rs` tests |
| FIFO, targeted cancellation, steering and approval/grant binding | `service/tests/thread.rs`, `service/tests/steering.rs` |
| Owner fencing, shared workspace cleanup and process loss | `service/tests/ownership.rs`, `service/workspace.rs` tests, `service/tests/process_recovery.rs` |
| Safe reconstruction/continuation without completed-call replay | `service/tests/recovery.rs`, `service/tests/startup.rs` |
| Durable public cutoffs, post-commit publication and slow observers | `service/tests/observation.rs` |
| Format refusal, unload/reload, bounded cold queries and directory filtering | `service/tests/unification.rs` |
| SQLite transaction/reopen/process-loss semantics | `apps/bitrouter/src/agent_store.rs` tests |
| Local/HTTP daemon process and receipt retry behavior | `apps/bitrouter/tests/native_agent_process.rs`, `agent_local.rs` tests |
| Native navigation, approvals, drafts, reconnect and terminal lifecycle | `apps/bitrouter/tests/native_agent_tui.rs`, `native_code.rs` tests |

All service paths are relative to
[`crates/bitrouter-orchestrator/src/`](../crates/bitrouter-orchestrator/src/).
The adapters use the same service. Test-layer overlap protects different failure
boundaries; this documentation change removes no tests or fixture coverage.

## Six-tool provider and platform evidence

[Tool acceptance](BRO_BASE_TOOLS_ACCEPTANCE.md) separates these cohorts:

| Cohort | Evidence and limits |
| --- | --- |
| Original six-tool experiment, 2026-10-01 | Four macOS real-model scenarios and historical Linux/macOS/Windows CI; source/binary provenance in the original manifest |
| Thread/Turn tool integration, 2026-10-04 | macOS real-model coding/read-only, source hashes and local suite at the recorded integration; distinct from later UI/main changes |
| Final Conversation/main integration | The latest local fixture suite above; earlier provider/CI results are not reattributed to this source |

Compact manifests and scenario summaries remain readable in
[the evidence directory](evidence/bro-base-tools/README.md). Full committed
exports, fixture outputs, cleanup records and historical CI detail are preserved
byte-for-byte in indexed compressed archives. Reproduction uses the existing
probe; its newly generated output is independent of archived past runs.

## Historical checkpoints

Historical records are retained at immutable Git revisions rather than appended
as competing current contracts. These results do not establish later revisions.

| Checkpoint | Retained result | Complete record |
| --- | --- | --- |
| Initial native server | Earlier P0–P6 implementation and later tool/streaming follow-ups | [Native phase ledger](https://github.com/bitrouter/bitrouter/blob/65555b126e8d14982b2a7c977b618d545cd8f5b7/docs/BRO_NATIVE_AGENT_IMPLEMENTATION.md) |
| Runtime v0.2 at `ce7a1291` | 102 orchestrator tests; workspace 3,677 passed/22 skipped; doc tests 5/1; strict checks passed locally | [Runtime phase ledger](https://github.com/bitrouter/bitrouter/blob/65555b126e8d14982b2a7c977b618d545cd8f5b7/docs/BRO_AGENT_RUNTIME_IMPLEMENTATION.md#evidence-so-far) |
| Thread/Turn unification | Workspace 3,688 passed/22 skipped; doc tests 5/1; strict checks passed locally; controlled provider fixtures | [Unification ledger](https://github.com/bitrouter/bitrouter/blob/65555b126e8d14982b2a7c977b618d545cd8f5b7/docs/BRO_THREAD_TURN_UNIFICATION_IMPLEMENTATION.md) |

The older runtime ledger retains transient CLI version-probe failure and its
unchanged-test rerun, toolchain compatibility issues and earlier incomplete gates.
The unification ledger retains the preceding leak warning and rechecks. Local
`/tmp` paths in historical documents identify original run locations; they are
not durable artifact download links. Only committed exports are in the archives.

## Reproduce and remaining gates

Use the repository's required workspace tests, Clippy and formatting checks for
source changes. Runtime checks are deterministic; real-provider tool checks use
[the documented probe](BRO_BASE_TOOLS_ACCEPTANCE.md#reproduce) and consume usage.
Archive verification is offline and documented in the evidence README.

Core/harness integration, inbound native ACP, native multi-agent scheduling,
adaptive model/context choices, OS isolation, power-loss guarantees and operator
resolution of lost owners/unknown effects remain separate. Unknown effects or
accounting and abruptly lost owners stay blocked; this record grants no authority
to delete safety records, retire an owner or restart execution. New validation
must identify its source, environment, outcome and limitations once, with focused
records linked from here instead of another cumulative phase ledger.
