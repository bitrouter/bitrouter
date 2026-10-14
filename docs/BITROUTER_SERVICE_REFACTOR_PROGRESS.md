# BitRouter service refactor progress

Implementation follows the [P0 plan](BITROUTER_SERVICE_REFACTOR_PLAN.md).
P1 remains out of scope. This ledger records evidence for the full P0 delivery;
completing one extraction does not complete the milestone.

## Baseline

- Source commit: `9956a427bd5d4316b3bf2359f88e293b158781d5`, also the remote
  `main` head at implementation start.
- Baseline packages: `bitrouter`, `bitrouter-ai`, `bitrouter-sdk`,
  `bitrouter-orchestrator`, `bitrouter-tui`, `bitrouter-telemetry`,
  `bitrouter-guardrails` and unpublished `dist-helper`.
- Application root inventory: 72 modules; the plan maps all package roots and
  their descendant ownership.
- Existing native runtime format: 7; the version validator accepts 2–7. Historical payload decoding still requires independent serialized fixtures.
- Database migration sequence: existing `m20240101_000001` through
  `m20240101_000027`, with the intentionally unused `000014` slot.
- Existing product executable: `bro`; the package remains `bitrouter`.
- Local validation platform: macOS arm64. Baseline compiler: Rust/Cargo 1.99.0.
  Strict Clippy uses the repository CI's pinned Rust 1.97.0 because the current
  compiler has a documented async-trait lint false positive.
- Full workspace normal/build tree: 596 distinct rendered entries, obtained with
  `cargo tree --workspace --all-features --edges normal,build --prefix none`.
  This is a reproducible tree baseline, not a count of unique package identities.

The initial full-suite command is:

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo nextest run --workspace --all-features \
  --build-jobs 2 --test-threads 2
```

Debug symbols are disabled consistently for local baseline and migration builds
to bound link/disk pressure. This setting does not disable tests or change runtime
feature selection.

The first baseline run stopped after 1,755 tests: 1,754 passed and the PTY
`code_agents_navigation_keeps_normal_buffer_and_defers_foreground_output` timed
out. That case passed in the subsequent run. The subsequent run is not a clean
baseline: three MCP fixture tests invoked Cargo while migration edits were in
progress, and encountered a temporarily invalid manifest. That run was stopped;
a fresh, detached source worktree at the baseline commit now runs the complete
suite independently of implementation edits. That frozen run completed:
**4,473 passed, 22 skipped, 6 slow**, in 784.349 seconds. No test was excluded
to obtain this result; skips are the baseline configuration's existing skips.

A baseline executable is retained outside the source worktree for later
process/performance comparisons. The initial build is 322,416,544 bytes with
SHA-256 `7336ae5ab9e36fd4af5e4fc78766ac0ad914f04b6d639ee3c6b9ec913cfccf6b`.
This is a debug-symbol-free development build, not a release-size claim.
The observation artifact hash is
`0b316b6afd7098e62db397000f2f725f4c9c88408ad73e58e08f65b03604ac0b`;
the contract move must preserve its bytes.

## Stages

| Stage | Status | Evidence or remaining work |
| --- | --- | --- |
| P0 0 Compatibility baseline | In progress | Frozen full baseline: 4,473 passed, 22 skipped; independent persisted payload fixtures and performance measurements pending |
| P0 1 Public contracts | In progress | Observation/event/request-check contracts and operation scopes moved to AI; matcher detached from old SDK; legacy adapters moved to application; Observation/capability slice verified; shared control/core DTO extraction remains before their final service/client split |
| P0 2 Application consolidation | In progress | Core/harness, telemetry and TUI merged; shared command/permission/presentation extraction verified; terminal-free transcript formatting remains |
| P0 3 Gateway and lightweight AI | In progress | Old SDK service/configuration/runtime moved to application; schema generator delegates to its application owner; AI runtime extraction and dependency isolation remain |
| P0 4 HTTP SDK | Pending | Typed gateway/control client and independent process consumer |
| P0 5 Feature isolation | Pending | Complete isolated build matrix and runtime availability checks |
| P0 6 Delivery and review | Pending | Regression checks, migration notes, packaging, independent review and PR |

## Acceptance ledger

Every requirement in the plan remains open until linked evidence demonstrates it
on the resulting implementation.

| Gate | State |
| --- | --- |
| A01 Package and module ownership | Pending |
| A02 Lightweight AI public contract boundary | Pending |
| A03 SDK client dependency boundary | Pending |
| A04 Independent official gateway process | Pending |
| A05 Feature dependency closure | Pending |
| A06 Runtime isolation | Pending |
| A07 Protocol and conversion parity | Pending |
| A08 Lifecycle parity | Pending |
| A09 Persisted data compatibility | Pending |
| A10 Core and harness authority | Pending |
| A11 CLI and TUI parity | Pending |
| A12 Telemetry behavior and names | Pending |
| A13 Extension and legacy guardrail behavior | Pending |
| A14 Build bootstrap and packaging | Pending |
| A15 External Rust API migration guidance | Pending |
| A16 Comparable performance measurements | Pending |

Known out-of-tree hosts consume the old builder/server, telemetry exporter and
legacy guardrails adapters. They require an explicit import and process-boundary
migration guide. Their external release status must not be inferred from local
workspace tests.

## Review evidence

P0 0 independent review identified four gaps, now included in the plan:
real legacy plugin installation tests; behavioral tests for isolated core/harness
profiles; a gRPC-only collector test; and historical payload fixtures distinct
from version-number acceptance. The two new plugin installation tests passed
against the original adapter before relocation, proving static input denial,
stream block/redact, hook order and per-request rule isolation. The added
gRPC-only collector baseline also passed before exporter relocation. It exercises
the actual exporter constructor, trace/metric delivery, static authentication
metadata and shutdown flushing in an isolated process. Persisted historical
payload baseline evidence remains open.

P0 1 independent review found one stale SDK observation rustdoc link in
`metrics.rs`. It now links to the AI contract and describes host seams accurately.
The review found no other actionable ownership, activation or behavior issue.
Current validation:

- `cargo check -p bitrouter-guardrails --all-features`: passed.
- `cargo check --workspace --all-features --all-targets`: passed.
- Relevant nextest run: **1,495 passed**, including AI/SDK/matcher/telemetry
  suites, moved plugin-installation tests, request-check real-process hosts,
  observation hierarchy, metering and session identity. The filter deliberately
  excluded unrelated application/agent suites; this is not final P0 coverage.
- Matcher all-feature normal/build tree contains no `bitrouter-sdk`.
- Default application normal/build tree contains no matcher.
- Observation artifact retains the baseline SHA-256 above.
- Formatting and diff whitespace checks: passed.
- Strict workspace rustdoc: passed.
- Workspace doctests: passed (6 passed, 1 ignored across the workspace).

AI still contains its original model runtime at this intermediate stage; complete
lightweight dependency isolation is a P0 3 gate, not a claim of this extraction.


P0 2 consolidation is in progress. Orchestrator and telemetry relocations each
passed workspace all-feature/all-target type checking. The relocated gRPC-only
collector test passed with HTTP disabled, including child-process trace/metric
delivery, static metadata and shutdown flushing. Independent static review found
one release blocker: CI still named retired packages. Those checks now target the
application and the final P0 dependency matrix; the matrix remains an open gate
until full feature isolation lands. The review found no other relocation issue.
TUI consolidation and shared command/permission/presentation extraction passed
all-feature/all-target checking. The first 1,037-test consolidation run passed
1,035 cases; two child-process recovery fixtures selected their former module
names and exited before fault injection. They now derive the test name from
`module_path!`, and both passed in a subsequent 226-test run together with the
active TUI/shared presentation tests and new source-boundary guards.

Restricting TUI visibility exposed unused legacy library APIs: the former
machine/picker/view, footer helpers and frame scheduler had no product callers.
Their removal retains the canonical Code view and its behavior tests, moves
resolver tests to shared commands, and replaces the headless permission effect
interpreter with direct use of the same harness policy/outcome resolution.
The 264 baseline renderer tests become 220 active renderer/shared tests; 44 tests
of the retired APIs are removed. Independent comparison against the baseline
found no removed product caller, consent/lifecycle regression, or unique active
behavior coverage lost. This is an alpha library API retirement, not a change to
the product's interaction paths. Session log-tail text now lives in presentation
without terminal types. Plain transcript Markdown formatting still requires a
terminal-free split.

The all-feature/all-target workspace check passed without project warnings.
Strict workspace rustdoc passed after replacing two public links to private TUI
implementation details. The subsequent real ACP/PTY, native-agent TUI, log-tail
and source-boundary run passed all 75 selected cases, including headless denial,
approval policy, streaming text output, cancellation and terminal restoration.

The plan review also identified two feature-isolation prerequisites: gateway
serving must retain policy-key projection and published-artifact validation; and
harness-only recovery must retain shared snapshot decoding and immutable-history
validation. Their ownership and cross-profile acceptance fixtures are now
explicit in the implementation plan. Both remain implementation work.

P0 3's old SDK service relocation moves assembly, HTTP routes, configuration,
pipeline, routing, MCP, extension registration and observation runtime into the
application. The application and distribution helper no longer depend on the old
SDK server framework. The SDK package is temporarily reserved for P0 4's client;
its final API guard and typed HTTP surface remain open.

The targeted gateway/configuration run passed **766 tests**, including all six
relocated integration suites. Independent review found that those six targets
were absent from Ubuntu's explicit CI shards and that removed reexports left
unresolved Rustdoc links. The targets now have a gateway shard and documentation
links name their defining modules. Strict workspace Rustdoc passed. The first
doctest run found a missing import in the relocated DispatchExecutor example;
the example now imports its application-owned executor and routing types.
The repeated workspace doctest run passed **5 tests**, with the existing one
ignored protocol sketch. Formatting and diff whitespace checks also passed.

The unchanged `dist-helper generate-schema --check` command now executes the
application-owned schema generator. It passed against the existing committed
artifact, preserving its bytes. Registry bootstrap remains independent of the
application's Cargo dependency graph. Clean-checkout and final feature matrix
validation remain open P0 gates.
