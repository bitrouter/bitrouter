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
| P0 3 Gateway and lightweight AI | In progress | Old SDK service and AI model/provider runtime moved to application; AI contract isolation verified; serving-time workflow/policy separation and final public API guards remain |
| P0 4 HTTP SDK | In progress | Typed gateway HTTP/SSE client and public wire response contracts verified; control/eval/core client extraction and independent official-service process consumer remain |
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

The next P0 3 slice moves catalog fetching, snapshot storage, disk-cache policy
and bundled provider baselines into application `gateway::catalog`. AI retains
only the catalog artifact types. Runtime retirement filtering now occurs in the
gateway's decoder for cached/bundled data and fetched provider artifacts, rather
than inside the public DTO's deserializer. The old quarantine fixture moves with
that policy; existing cache tests still require the original file bytes to remain
unchanged. Endpoint applicability for published regional pricing moves to its
gateway routing caller, leaving the profile value in AI.

Catalog validation passed the workspace all-feature/all-target check and strict
Rustdoc. The targeted catalog/configuration/Decisions run passed **277 tests**,
including source binding, historical retirement quarantine, offline behavior,
partial refresh/persistence failures, cancellation, bundled fallback and endpoint
pricing applicability. Independent static review found no material issue. The
remaining AI model/authentication/protocol runtime is still outside the target
contract-only boundary and remains P0 3 work.

The following P0 3 slice moves that remaining runtime into application
`gateway::model` and `gateway::providers`: upstream clients, protocol codecs,
conversion admission, streams, credentials, authentication and provider sessions.
AI keeps model semantics, 18 public request structures/unions, conversion report
data and the existing lifecycle/observation/capability/catalog contracts.
Credential-bearing targets and their header-selection `AuthScheme` are now
application-owned. Catalog compatibility values remain public artifact data.

AI's production dependencies are now serde, serde_json and thiserror, with
schemars behind `schema`. The former authentication/runtime features are removed.
Isolated default and all-public-features trees contain no network client,
executor, database, terminal or OTel dependency. Independent contract tests
passed **32 default** and **36 all-feature** cases; strict all-target/all-feature
AI Clippy passed. These checks do not replace the final API guards after the
remaining service DTOs are extracted for the SDK.
The matcher also passed its isolated all-feature run (**15 tests**) without
enabling the application or relying on workspace-wide AI schema activation.

All **543** named AI tests at the stage base retain a destination. Seventeen
runtime integration targets move into the application, with explicit Ubuntu CI
coverage; schema snapshots and their tests stay beside AI's wire definitions.
The first selected application/contract/matcher run passed **1,709 tests**,
including model protocols, conversion, auth/login/files, provider integrations,
continuations, configuration, hosted account behavior and telemetry.

Independent review found that newly serializable request DTOs could erase
message-level replay metadata before gateway admission. Nested request objects
now retain unmodelled fields; message Debug hides those opaque fields. Two new
regressions compare gateway semantics before/after typed serialization and prove
that rejected replay evidence retains the same refusal. The follow-up protocol
and contract run passed **334 tests**; focused independent re-review found no
remaining material issue. Adding Serialize also exposed previously unpublished
schema defaults; schema attributes preserve the prior structural snapshots.
The only snapshot edits replace the obsolete consumer-specific description.

Workspace all-feature/all-target checking passed. Strict workspace Rustdoc and
doctests passed after repairing moved module links (**5 examples passed, one
existing protocol sketch ignored**). The application-owned configuration schema
generator passed its unchanged-artifact check. Full P0 feature isolation,
independent service-client integration, packaging and final validation remain
open; this stage does not claim live-provider or cross-platform verification.


P0 4 now provides the gateway HTTP client, with explicit endpoint/credential
options, bounded JSON and SSE reads, separate protocol wire responses/events,
and native Decisions usage. Discovery, health and metrics are included. The
client disables redirects and retries; environment/system proxies require an
explicit option so an ambient proxy does not become an implicit service hop.
It preserves HTTP status/headers even when an error body cannot be consumed.
Responses failure lifecycle events remain typed terminal responses; top-level
stream errors and premature EOF are distinct client failures.

Isolated SDK verification passed **16 real HTTP tests** and the README's compiled
example. These tests use Rust HTTP/TCP fixtures without importing the application.
They cover stream/nonstream calls, nested metadata, usage, BOM and UTF-8 framing,
response/event limits, redirects, errors, truncation and a disconnected POST
that is sent only once. An initial disconnect check passed through an ambient
system proxy and observed its 502; explicit direct transport made this fixture
hermetic. This is client transport evidence, not the final independent-process
acceptance test against the released gateway artifact.

The gateway protocol and boundary run passed **285 tests**, including new
response/SSE-to-contract round trips and the existing native reasoning fixture
with explicit nulls. AI's isolated all-feature suite passed **36 tests**.
AI/SDK strict all-target Clippy and strict workspace Rustdoc passed. The isolated
normal/build trees retain AI's contract-only dependencies and exclude application,
server, ORM, ACP, terminal and OTel dependencies from SDK production code.

Independent review identified parser behavior on BOM/invalid UTF-8, native null
preservation and loss of HTTP metadata after error-body failures. The SDK now uses a private bounded SSE framer with one shared line/event
byte budget, validates UTF-8, removes only one initial BOM, and retains native
nullable fields and body-read failures alongside HTTP metadata. New wire DTO
diagnostics redact opaque payloads. Regression tests exercise each correction.

The existing `sdk-public-api` CI check now renders both AI and SDK with the
shared pinned nightly/tool. Positive sentinels verify the real contract/client
surface before runtime-signature and public-reexport checks. The exact CI script
passed locally against the generated dependency manifests: AI exposes schemars,
serde_core and serde_json; SDK exposes bitrouter_ai, futures_core, http and
reqwest. SDK no longer declares server/ORM/ACP/OTel public dependencies. Further
control/eval/core DTO extraction must preserve these boundaries or deliberately
review an updated manifest. Full P0 acceptance and the final PR remain open.


Focused independent re-review found no remaining actionable issue in the final
SSE framing, presence-aware DTOs, diagnostic redaction or public API guards.
The final SDK/AI run passed **16 HTTP tests, 36 contract tests and one SDK
doctest**; the separate application run again passed **285 tests**. Final
AI/SDK all-target strict Clippy and strict Rustdoc passed. The final full-workspace all-feature/all-target check also passed. The SDK
package alone again passed all 16 HTTP tests and its doctest without relying on
workspace-wide AI schema activation. macOS development linking emitted an
oversized unwind-table warning for the existing full application, separate from
source lints and the passing test results. Release performance and final P0
validation remain open.
