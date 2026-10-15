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
| P0 1 Public contracts | In progress | Observation/event/request-check, control, eval, managed-attempt evidence and core protocol/checkpoint contracts moved to AI; matcher detached from old SDK; legacy adapters moved to application; final public-consumer coverage and service/client isolation remain |
| P0 2 Application consolidation | In progress | Core/harness, telemetry and TUI merged; shared command/permission/presentation extraction verified; checkpoint codecs, persisted snapshot records and offline context-history checks detached from scheduler modules; independent profile wiring and terminal-free transcript formatting remain |
| P0 3 Gateway and lightweight AI | In progress | Old SDK service and AI model/provider runtime moved to application; AI contract isolation and current public API guards verified; serving-time workflow/policy separation and final dependency matrix remain |
| P0 4 HTTP SDK | In progress | Typed gateway HTTP/SSE, control, eval and core HTTP/WebSocket clients verified; remaining harness API consumer extraction and independent packaged-service process consumer remain |
| P0 5 Feature isolation | In progress | Scheduler/local native bridge gated by core; core-disabled inspection and early rejection verified; complete isolated dependency/startup matrix remains |
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

P0 4's control-contract prerequisite moves **21 public types** into AI
`control` and `control::reload`: scopes, capabilities, resource/action
descriptors, configuration-state evidence, reload inputs/outcomes and operation
reports. Application configuration, CLI reports, daemon IPC and control handlers
now import their owning definitions directly. Runtime timestamp capture, reload
participant mutation, capability assembly, UUID admission and operation ownership
stay in the application. No client or execution runtime was added to AI.

The contract fixtures cover legacy capability advertisements without inferred
grants, flattened state with independently sampled generations and mixed-state
history, strict reload input, and unknown operation outcomes with recovery IDs.
AI default tests passed **35 cases** and all-feature tests passed **39 cases**.
Its strict Clippy, Rustdoc, isolated dependency tree and pinned public API
checks passed; the public foreign-dependency manifest is unchanged. CI now
requires the reload input contract in the generated API listing as well.

The initial application run passed 226 control/reload/daemon/configuration tests.
Strict workspace Clippy exposed earlier relocation leftovers: a nested
`pipeline::pipeline` module, a schema helper after its test module and an
identity closure in the contributor schema launcher. The implementation file is
now `gateway::pipeline::execution`, unchanged apart from owning paths; the two
schema fixes preserve behavior. Compilation caught two missed alias-based
pipeline paths, which were corrected and independently re-reviewed.

Final expanded validation passed **600 application tests**, covering control,
reload, configuration/schema, daemon, custom-host lifecycle, guardrails migration,
model pipeline and Decisions. The other 2,815 tests were outside this stage's
filter; this is not a full P0 test result. Strict workspace all-target/all-feature
Clippy on Rust 1.97.0 and strict workspace Rustdoc passed. The standard
`dist-helper generate-schema --check` passed; schema SHA-256 remains
`d4aaacc1d5e9b162cd168c08cc4de2b495ba9fbec3b1c582999684b2a1883135`.
Registry artifacts also match the committed baseline. The existing macOS
development-link unwind-table and dependency future-incompatibility notices
remain separate from the passing source lint and behavior gates.

Independent review found no remaining actionable contract or relocation issue.
The implementation plan also incorporates the reviewed gateway ownership of
published experiment assignment and certificate validation, plus the complete
consumer migration table and request/response/event fidelity gates. Control
inspection DTOs and the portable HTTP client remain in the application pending
the next P0 4 extraction; eval/core clients, final feature isolation and the
complete P0 acceptance/PR remain open.

P0 4 now also moves **42 inspection and shared routing types** into AI:
model/router inventory, route previews and pricing evidence, provider/agent/
policy/observation reports, status/spend/handoff activity, request filters and
request-history views. Published routing preferences and policy model targets
are shared data contracts, not a routing implementation. Their field names,
serde rules and schema definitions remain intact. Intrinsic identifier grammar
returns a small typed error; policy selection against loaded configuration,
request-window resolution against the clock, storage reads, local-version
compatibility and CLI presentation remain application-owned.

Private extension traits keep those application operations on shared reports
without exposing service types through AI or introducing forwarding exports.
The legacy request-report constructor has only repository test callers; it and
its window helpers are now test-only and continue delegating to the production
`new_filtered` constructor. No existing test bodies or expected outputs were
removed to make the extraction pass.

Validation for this extraction passed **953 tests** (914 application and 39 AI),
with 2,500 other tests outside the selected stage filter. This covers control,
reload, configuration, actions/report formatting, routing/policy, model pipeline
and the selected real-process daemon/configuration suites. Isolated AI default
tests passed **35 cases**. Strict workspace all-feature/all-target Clippy and
strict workspace Rustdoc passed. The pinned AI/SDK public API scan passed with
unchanged dependency manifests; the normal/build AI closure still contains only
serialization, optional schema and error-support dependencies. The new request
filter and status contracts are also positive CI API sentinels.

The standard configuration schema check passed with unchanged SHA-256
`d4aaacc1d5e9b162cd168c08cc4de2b495ba9fbec3b1c582999684b2a1883135`;
the observation schema also retains its baseline bytes. Formatting and patch
whitespace checks passed. macOS development linking again reported the known
oversized unwind table; no source-lint or behavioral failure was suppressed.

Independent review compared all 42 declarations and their runtime method
separation against `ca98b68` and found no actionable issue. The implementation
plan now explicitly retains trajectory progress-guard evaluation, its health
reducer and required persisted-state validation in the gateway; isolating core
must not disable published policies. The HTTP control client still remains in
the application for the next extraction. Eval/core client work, the complete
feature matrix, compatibility fixtures, final P0 validation and PR remain open.

A read-only history fetch found upstream `main` at `2fd95b1`, one catalog/test
refresh commit beyond the frozen baseline. The merge preserves the catalog
refresh and maps the native-agent test synchronization fixes to their harness
destination. Independent review found all 32 upstream changes retained and no
resurrected package or stale ownership paths. Registry validation and generated
artifact checks passed; the 32 advisory messages concern existing non-curated
models and unpinned/unverified external agent entries, not validation failures.
The focused run passed **94 tests**, including the policy/native-agent tests and
the resized/suspended agent-navigation PTY case. Baseline compatibility evidence
remains tied to `9956a42`; the catalog refresh is an explicit upstream change.
The two other affected suspend/resume and standalone-manager/attach PTY paths
also passed in a focused run. These cases build their process fixture and were
reported as slow; no retry or assertion relaxation was used. Formatting and
patch whitespace checks passed after integration.

Historical writer inspection also identified concrete source revisions for
future persisted-payload fixtures. These are provenance candidates, not claims
that fixtures have already been generated or replayed:

| Runtime format | Inspected writer revision |
| --- | --- |
| 2 | `e2453004734b1a2723b5f7e54087e21f3e64a315` (PR 945, Thread/Turn execution) |
| 3 | `321ccaad5fe69f7adb39e0230a11f6e1851ff84d` (PR 945, harness resources) |
| 4 | `6298d781b12883dbc41c913087cd46e9e3c637f9` (PR 945, scoped instructions) |
| 5 | No writer found in the inspected lineage; `a68c215` advances 4 directly to 6 while accepting 5. Its payload provenance remains an open evidence item. |
| 6 | `a68c215f0b579aeb34dd15baf1670ef037c310f8` (PR 961, core checkpoints/artifacts) |
| 7 | Frozen baseline `9956a427bd5d4316b3bf2359f88e293b158781d5` (PR 963, native ACP) |


### P0 4 Control HTTP client migration

Starting from `6256262`, portable control transport now lives in
`bitrouter_sdk::control::ControlClient`. AI owns action IDs/version/scope metadata,
the minimum credential length and the public control error envelope. The server
inventory consumes that same metadata. The application client adapter retains
saved-profile/environment credential selection, its previous proxy behavior and
CLI error classification; it no longer implements HTTP dispatch or capability
caching. Standalone SDK callers provide endpoint and credential values explicitly
and opt into system proxies only when needed.

Capability/version/scope checks, legacy read-only discovery, boot/error cache
invalidation and guarded reload recovery retain their previous behavior. Reload
submission is never retried; a failed receipt or poll retains the original
request and instance IDs. Non-success HTTP status and headers survive malformed,
oversized and truncated bodies. The SDK guide now includes a compiled control
example and explicit recovery guidance, and CI guards both new public surfaces.

Validation used `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`, with two build jobs and test threads:

- `cargo nextest run -p bitrouter -p bitrouter-ai -p bitrouter-sdk --all-features
  --build-jobs 2 --test-threads 2 -E 'package(bitrouter-ai) | package(bitrouter-sdk)
  | test(remote_control::) | test(contexts::) | test(administration_target::)
  | test(reload::)'`: **115 passed**, including 40 AI contracts, 25 SDK HTTP
  cases and 50 application tests; 4,309 tests outside the stage filter.
- `cargo nextest run -p bitrouter --all-features --build-jobs 2 --test-threads 2
  -E 'test(code_remote_) | test(official_cli_uses_shared_host_without_extension_registrations)
  | binary(configuration_state)'`: **7 passed**, including both remote TUI
  paths and the official executable's service lifecycle; 4,352 outside the filter.
- `cargo test -p bitrouter-ai --no-default-features --jobs 2`: **36 passed**.
- `cargo test -p bitrouter-sdk --doc --jobs 2`: **2 examples compiled**.
- `cargo +1.97.0 clippy --workspace --all-features --all-targets --jobs 2 --
  -D warnings` and strict all-feature workspace Rustdoc: passed.
- The pinned public API check passed with unchanged foreign-dependency
  manifests. Isolated AI and SDK normal/build trees contain no forbidden
  service, database, UI or OTel dependencies; AI also excludes client/async
  runtimes. Formatting and patch whitespace checks passed.

Independent review compared the migration with the prior application client and
found no actionable issue. Known macOS development-link unwind-table and
third-party future-incompatibility notices remain separate from source checks.
The plan also now explicitly retains decision policy/pricing validation in the
gateway and requires independent client acceptance against packaged artifacts.

These results complete the control-client extraction, not the full P0 SDK or
service delivery. Eval/core clients, the separate SDK consumer process fixture,
feature isolation, historical payload/performance evidence, final validation and
PR remain open. The existing official-executable test does not substitute for
the planned independently built SDK consumer.


### P0 4 Evaluation exchange contracts and typed client

Starting from `e8023ce`, **21 existing evaluation data types** and their schema
version constants now live in `bitrouter_ai::eval`. Their fields and existing
serde rules were compared directly with the previous declarations. The subject
insert receipt, snapshot request, status report and HTTP error body now have
explicit shared types. The existing `heldout` status-count key remains distinct
from the `held_out` result-receipt value. The definition-only `EvalSubjectStatus`
enum had no source or test consumers and was retired instead of being added to
the public contract package.

Metric range checks are intrinsic contract helpers with typed errors. Evidence
validation, SHA-256/canonical digest construction, route measurement construction,
tenant authority, persistence and admission remain application code. Existing
callers use owning paths and service-side free functions rather than forwarding
exports or an AI dependency on server algorithms. Existing tests retained their
assertions and inputs; only owner paths and relocated helper calls changed.

`GatewayClient` now supplies the seven existing eval HTTP operations on the
inference listener, with no new client feature. The server continues to authorize
virtual keys and tenant ownership, or its explicitly configured `skip_auth`
mode. Control credentials do not authorize eval calls. Resource identifiers are
encoded as path segments, and a missing endpoint remains an HTTP error.
`Error::Evaluation` retains the distinct string-valued error envelope plus
status/headers even for unreadable bodies. Gateway and eval reuse bounded
response handling; redirects and automatic retries remain disabled.

Validation uses the same debug/incremental settings and two-job/thread limits
recorded above:

- `cargo nextest run -p bitrouter -p bitrouter-ai -p bitrouter-sdk --all-features
  --build-jobs 2 --test-threads 2 -E 'package(bitrouter-ai) | package(bitrouter-sdk)
  | test(eval::) | test(policy_lock::) | test(policy_table_router::)
  | test(workflow_state::) | test(optimization::) | test(trajectory::)
  | binary(eval_exchange_equivalence) | binary(policy_eval_control_plane)
  | binary(workflow_state_replay)'`: **688 passed** (619 application, 42 AI,
  27 SDK); 3,742 outside the selected stage filter.
- New real-HTTP SDK tests exercise server receipts, duplicate identities,
  encoded subject IDs, snapshot identity, virtual-key authentication and tenant
  isolation. Independent SDK HTTP peers cover unavailable/malformed/oversized
  errors and path-prefix preservation. These complement the existing CLI/REST
  semantic-equivalence test.
- Strict Clippy found one unnecessary clone in a new test assertion. After
  replacing it with a borrowed slice, the final focused run of
  `binary(eval_exchange_equivalence) | test(remote_control::)` passed **24 tests**;
  strict workspace/all-feature/all-target Clippy passed on Rust 1.97.0.
- AI default-feature tests passed **38 cases**; both SDK README examples compile.
  Strict all-feature workspace Rustdoc passed on Rust 1.97.0.
- Pinned public API checks passed with new eval contract/client sentinels and
  unchanged foreign-dependency manifests. No package dependency was added.
  Formatting and patch whitespace checks passed.
- `cargo run -p dist-helper -- generate-schema --check` passed. Configuration
  schema SHA-256 remains
  `d4aaacc1d5e9b162cd168c08cc4de2b495ba9fbec3b1c582999684b2a1883135`;
  the observation artifact also retains its baseline bytes.

Independent review found no introduced contract, digest, authority or transport
issue. These checks complete the eval client extraction, not P0 as a whole.
Core protocol/checkpoint extraction and streaming clients, the independent
packaged-service consumer, feature isolation, historical payload/performance
fixtures, final complete validation and PR remain required. No P1 implementation
or new evaluation/policy behavior is included in this stage.

## Serving-boundary review additions

The accepted plan now explicitly retains gateway-side reserved-request replay
checks and accounting protection when core is disabled. It also separates
trajectory settlement, operational envelope construction and durable outbox
writes from core evaluation admission/publication. Existing and new outbox rows
must remain inspectable and pending without a consumer, and drain idempotently
when core returns. These are open P0 3/P0 5 and A09 requirements, not verified
feature-isolation results.

The documentation review checked these splits against the baseline and found no
remaining material issue after revision. P1 separately includes shared
correlation-key/epoch authority and coordinated episode/guard updates; those
requirements do not add distributed work to P0.

## Managed attempt evidence contracts

The existing `model.evidence` command carries a complete managed provider-attempt
report. Its 24 reachable report, route, token/cache, private-context and
continuation data types now live in `bitrouter_ai::core::evidence`. All service,
core, harness and test consumers import their actual owner. No forwarding
re-export or new dependency was added.

Provider usage projection, exact-byte commitment generation, report admission,
private-context authentication and execution callbacks remain application code.
Only intrinsic data constructors/defaults and field queries accompany the moved
types. The two existing configuration-schema derives remain available through
AI's optional `schema` feature. Application helper replacements are documented
in the development guide.

Validation against the source immediately following `a45eaff`:

- A declaration comparison verified all 24 field sets, serde attributes and
  trait derives; only ownership paths and optional schema gating changed.
- Workspace all-features/all-targets `cargo check` completed. The final strict
  workspace all-features/all-targets Clippy run on Rust 1.97.0 passed after one
  unused test import was removed. No lint suppression was added.
- The targeted nextest run passed **1,110 tests**, including core contract and
  execution, the managed HTTP/WebSocket gateway, native provider/history,
  continuation, token accounting, configuration and AI contracts. Three tests
  were slow; 3,293 tests were outside the filter. The final cleanup removed only
  an unused import and was covered by the subsequent strict all-targets check.
- AI without default features passed **38 tests** on Rust 1.97.0. Strict
  workspace rustdoc and the pinned public-API guard passed. The public-dependency
  manifests remain unchanged; positive managed-evidence sentinels and negative
  execution/runtime/helper sentinels now protect the boundary.
- `cargo run -p dist-helper -- generate-schema --check` passed. The configuration
  and observation schema hashes remain the baseline hashes recorded above.
- Independent read-only review found no actionable issue with serde/default
  behavior, helper extraction, migrated callers, test assertions or the public
  dependency/authority boundary.

The nextest invocation used `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, and:

```sh
cargo nextest run -p bitrouter -p bitrouter-ai --all-features \
  --build-jobs 2 --test-threads 2 \
  -E 'package(bitrouter-ai) | binary(core_contract) | binary(core_execution) | binary(orchestrator_core) | test(gateway::pipeline::) | test(continuation::) | test(metering::native_cost::) | test(config::)'
```

This removes a dependency of the public core wire messages on gateway report
implementations. Core protocol/checkpoint extraction and the SDK's existing
core streaming transport still remain. Feature isolation, historical persisted
fixtures, packaged independent-process delivery, full P0 validation and the PR
remain open; this stage does not complete any broader gate by itself.

## Distribution-profile review additions

The accepted plan now requires compiled capability identity in daemon
compatibility evidence and explicit preflight for full/minimum profile
transitions. Same-revision builds must not claim equivalent capabilities or
silently replace each other. Existing active-work, ownership, rollback and
no-start restrictions remain applicable.

Updates must preserve the installed distribution profile, or leave it unchanged
with profile-specific manual instructions when a matching artifact is
unavailable. Custom feature builds must not silently receive the full artifact.
These requirements were checked against baseline build, handoff and updater
paths and independently reviewed. They remain open P0 5/P0 6, A08 and A14 work;
no implementation or packaged-update validation is claimed here.

## Managed core protocol and checkpoint contracts

The 42 existing public protocol/checkpoint data types now live in
`bitrouter_ai::core::protocol` and `bitrouter_ai::core::checkpoint`. The existing
HTTP error response also uses a typed `ErrorEnvelope`. Requests, responses,
capabilities, limits, ownership references, exact-byte checkpoint envelopes and
ACK data import their actual AI owner throughout the application and tests.
No forwarding module or new protocol was introduced.

The application retains manifest hashing, authenticated-grant checks, tool
result enforcement, checkpoint encoding/digest verification, durable append
admission, verified payload reuse and the commit gate. Application extension
traits attach those existing methods to the shared data types. Intrinsic
identity/digest grammar, declared-limit checks, defaults and data projections
remain with the contracts; parsing an envelope or constructing an ACK does not
establish durable authority. Private scheduler snapshots and shared recovery
codec feature isolation remain application work.

An isolated regression test reproduced a finite pricing value changing one bit
when decoded without the application's serde_json feature unification. AI now
enables `float_roundtrip` itself. That feature adds no dependency; the full
application already enabled it. Four new contract tests cover the regression,
opaque checkpoint byte preservation, command tags/revisions and HTTP unknown
commit status.

Validation against code baseline `2a1ef62`:

- Exact declaration comparison passed for all 42 moved types, including fields,
  serde attributes and derives. Five intrinsic protocol implementation blocks,
  including limit defaults/negotiation, compare unchanged.
- Workspace all-feature/all-target `cargo check` passed. Strict workspace
  all-feature/all-target Clippy passed on Rust 1.97.0, including after the review
  correction described below. Strict workspace Rustdoc passed on Rust 1.97.0.
- The targeted nextest run passed **618 tests**, with six slow tests and 3,789
  tests outside the filter. It exercised core contracts/execution, managed
  HTTP/WebSocket and process recovery, native harness journals, ThreadService
  ownership/approval/recovery, managed resources/storage and AI contracts.
- AI without default features passed **42 tests**. The pinned public-API guard
  passed with new protocol/checkpoint sentinels and explicit rejection of
  application validators, codecs and commit-barrier types. Public foreign
  dependency manifests remain unchanged.
- Configuration-schema generation/check passed; configuration and observation
  schema SHA-256 values remain the baseline hashes recorded above. Formatting
  and patch whitespace checks passed.
- Independent review identified one misplaced Unix attribute on an inserted
  test trait import. The trait import is now unconditional and the original
  `Path` import is Unix-only again. A scan found no other newly inserted trait
  import with this issue; independent re-review confirmed the fix. Local
  execution remains on the macOS environment recorded above.

The targeted command used the same local debug/incremental settings as the
preceding stage:

```sh
cargo nextest run -p bitrouter -p bitrouter-ai --all-features \
  --build-jobs 2 --test-threads 2 \
  -E 'package(bitrouter-ai) | binary(core_contract) | binary(core_execution) | binary(orchestrator_core) | binary(managed_native) | test(core::) | test(harness::agent::) | test(harness::resources::managed::) | test(harness::service::) | test(managed_store::)'
```

After the import correction, the focused ThreadService thread suite passed
**15 tests** (`cargo nextest run -p bitrouter --all-features --lib
--build-jobs 2 --test-threads 2 -E 'test(harness::service::tests::thread::)'`).
The final strict all-target Clippy run also passed.

This completes the core wire-data extraction. The SDK's core HTTP/WebSocket
client, remaining public-consumer migration, isolated feature/runtime profiles,
historical payload fixtures, packaged independent-process delivery, performance
comparison, full P0 validation and PR remain open. A01–A16 retain their final
milestone status; this stage does not substitute for those complete gates.

## Managed core SDK transport

Changes against `33c6e9d` add the existing core capability HTTP endpoint to the
base SDK and its managed WebSocket channel behind the SDK's optional `core`
feature. The client uses AI commands, events and errors directly. It does not
start a service, load credentials, persist or acknowledge checkpoints, execute
tools, reconnect or replay mutations. Split send/receive halves let an external
harness acknowledge durable work while receiving service events.

The channel has a dedicated HTTP/1.1 connection pool with the gateway client's
explicit authentication, proxy, TLS and handshake timeout policy. This prevents
HTTP/2 feature unification or a previously pooled HTTP/2 connection from changing
the upgrade protocol. Ordinary gateway HTTP uses its existing pool. Handshake
validation rejects unsolicited extensions/protocols, duplicate acceptance
headers and redirects; HTTP failures retain bounded typed error bodies, status
and headers. WebSocket frame/message input and outbound JSON serialization have
explicit limits. Public APIs do not expose tungstenite types.

Independent review found that cancelling a pending sink send could retain a
command that a later send, close or receiver-driven pong would flush. A real
Hyper HTTP upgrade over a bounded duplex transport reproduced this failure
before the fix. Both halves now share a concrete I/O wrapper: cancellation or
failure of a pending write drops the actual connection synchronously and wakes
pending readers/writers. Regression tests verify both cancelled sends and
cancelled close handshakes, reader wakeup, rejected subsequent operations and
the maximum prefix that reached the peer. Already dispatched bytes remain an
unknown execution outcome; connection invalidation does not prove cancellation.
A local serialization/size rejection happens before dispatch and does not
invalidate an otherwise usable channel.

The application integration uses its real managed API and authentication. It
receives an initial checkpoint without an automatic ACK, validates and persists
that batch in a separate harness directory, explicitly acknowledges it and
observes the resulting durable head. The existing service drops its WebSocket
after a client close without flushing the close acknowledgement; the test
preserves this behavior and expects an explicit incomplete-handshake error.
A separate controlled TCP peer verifies the client's successful close handshake.
No service execution or session ownership behavior changed.

Validation on the local platform recorded above:

- SDK all-feature tests passed **45 tests**, including two deterministic
  backpressure tests, 12 TCP channel tests, nine control tests, 19 gateway/eval/
  capability HTTP tests and three doctests. The no-default-feature SDK run
  passed **31 tests**, with channel-only tests correctly absent.
- The real application's `orchestrator_core` managed API filter passed
  **28 tests**, with 38 tests outside the filter. Coverage includes credential
  revocation, bounded provider output, ACK/reconnect recovery, process crashes
  and the new SDK checkpoint exchange. This is a targeted result, not the full
  P0 regression suite or the packaged independent-process consumer gate.
- Workspace all-feature/all-target `cargo check` passed. Strict workspace
  all-feature/all-target Clippy and strict workspace Rustdoc passed on Rust
  1.97.0. Formatting and patch whitespace checks passed.
- Isolated SDK normal/build dependency scans with and without `core` contain
  no application, server, database, terminal, ACP/MCP or OTel implementation.
  The default SDK excludes WebSocket dependencies. The pinned public-API guard
  passed with channel/capability sentinels and rejection of raw transport types;
  the public foreign-dependency manifests remain unchanged.
- Independent follow-up review found no remaining actionable issues after the
  cancellation and close-handshake corrections.

The targeted runtime command was:

```sh
cargo nextest run -p bitrouter --all-features --test orchestrator_core \
  --build-jobs 2 --test-threads 2 -E 'test(managed_api::)'
```

The implementation plan also incorporates the reviewed internal dependency
rules and final completion-record requirements. Independent-process acceptance
now explicitly supplies separate inference and control endpoints/credentials,
retains loopback-only control and checks that credentials are not interchangeable.

Remaining P0 work includes shared internal checkpoint/history decoding for
core-only and harness-only profiles, terminal-free headless rendering, the full
feature/startup matrix, remaining public harness consumer contracts, baseline
payload fixtures, packaged gateway delivery, performance budgets, final whole-P0
validation and PR. All final A01–A16 gates remain open until their complete
candidate evidence is recorded. P1 remains outside this implementation.

## Shared checkpoint codec extraction

Changes against `41ccd98` move exact-byte checkpoint encoding/decoding, bounded
wire-size checks, digest validation, verified-payload reuse and append validation
to `bitrouter::storage::checkpoint`. Shared harness-manifest, artifact and frozen
tool-result checks now live in `storage::checkpoint::validation`. Core retains
`CommitGate` and its acknowledgement/dispatch state, and `core::protocol`
retains checks against the live connection's authenticated grant. The existing
reserved context-tool names move below the scheduler boundary with manifest
validation; their values and dispatch interpretation are unchanged.

Core, harness journal/recovery, managed storage, response adapters and tests
import the actual owners without forwarding exports. Shared codecs do not
depend on scheduler/harness implementations or network/executor libraries.
They validate supplied facts without authenticating an owner, writing a journal
or granting permission to dispatch. The harness still atomically persists the
batch and fences before sending its ACK; only core's gate advances scheduling.
No public AI types, serialized fields or schema versions changed.

Validation evidence:

- Full implementation-body comparisons against `41ccd98` passed for CommitGate,
  all shared codecs/append checks, the retained ClientMessageExt implementation
  and the moved manifest/tool-result checks. The only implementation reference
  change is the declared owner of the identical reserved-name list.
- Workspace all-feature/all-target `cargo check` passed. Strict workspace
  all-feature/all-target Clippy and strict workspace Rustdoc passed on Rust
  1.97.0. Formatting and whitespace checks passed.
- Independent review found no code, cfg-attachment or commit-authority issues.
  Its sole finding was stale trait-import guidance in `DEVELOPMENT.md`; the
  paths and adjacent SDK status were corrected and independently confirmed.
- The targeted nextest run passed **589 tests**, with five slow tests and
  3,773 tests outside the filter, in 518.253 seconds. It exercised exact-byte
  append/replay, duplicate/lost ACKs, bounded archive/tool payloads, durable
  recovery, ownership and tool fences, managed HTTP/WebSocket, process crashes,
  provider continuations and the SDK checkpoint exchange. The source and
  manifests remained unchanged throughout the run, including nested Cargo
  invocations from harness fixtures.

The targeted runtime command was:

```sh
cargo nextest run -p bitrouter --all-features --build-jobs 2 --test-threads 2 \
  -E 'binary(core_contract) | binary(core_execution) | binary(orchestrator_core) | binary(managed_native) | test(core::) | test(harness::agent::) | test(harness::resources::managed::) | test(harness::service::) | test(managed_store::) | test(agent_store::)'
```

At this checkpoint, private `SessionSnapshot`, its persisted records and
immutable-history validation still required separation from online scheduling;
the following stage addresses that dependency. This codec move alone is not
evidence that a harness-only build or the final feature matrix already works.

## Shared private snapshot and offline history extraction

Changes against `2570f3b` move 69 persisted record declarations to
`bitrouter::storage::checkpoint::snapshot`. The complete private state remains
inside the application; neither AI nor SDK gains a snapshot or runtime
dependency. The actual data owners are:

| Previous core owner | Application storage owner below `storage::checkpoint::snapshot` |
| --- | --- |
| `session` records | Root module: `SessionSnapshot`, agent/run/step/invocation records and statuses |
| `allocation`, `routing`, `collaboration`, `signals`, `reconstruction` records | Corresponding record modules |
| `accounting`, `accounting::work` records | `accounting`, `accounting::work` |
| `session::{root_queue,steering,release,responses}` records | `root_queue`, `steering`, `release`, `responses` |
| `context_router` records and feature identifiers | `context` |
| `context_router::planner` records | `context::view` |
| `context_router::evidence` records | `context::evidence` |
| `context_router::tools::ContextAction` | `context::action::ContextAction` |
| Immutable context-history validation | `context::validation` |
| Bounded stored-state observation projection | `context::inspection` |

Intrinsic defaults, response emptiness, evidence commitments and exact UTF-8
span extraction, view identity, portable-evidence checks, task-tree inspection
and accounting getters accompany the records. Core retains live context
compilation/selection, evidence publication, collaboration application, cost
recording and session mutation. Existing inherent implementations remain with
their runtime owners in the same application crate. The only visibility
expansions are crate-local access to the shared context digest/error helpers and
the immutable evidence-source compatibility predicate.

Harness native journal/recovery, budget and observation code now imports the
storage owners directly, as do core, managed response adapters and tests. No
forwarding exports, checkpoint version changes, new serde fields or changed
defaults were introduced. Reserved feature strings, wait defaults and existing
serde callbacks retain their values and meanings. Stored records and offline
helpers import no core/harness implementations or Tokio, reqwest or Axum APIs;
their existing gateway data and pure validation/projection dependencies remain
inside the application.

Validation evidence:

- Comparison against `2570f3b` passed for 91 moved declarations/helpers and two
  complete offline modules, preserving fields, derives, serde attributes,
  defaults, predicates and diagnostics after normalizing qualified owner paths
  and formatting. The remaining 80 runtime/consumer/test files also matched
  after excluding imports and applying the same path/format normalization.
- Workspace all-feature/all-target compilation passed. Strict workspace
  all-feature/all-target Clippy and strict workspace Rustdoc passed on Rust
  1.97.0. Formatting and whitespace checks passed.
- Independent read-only code and documentation review found no actionable
  issues. It checked unique record ownership, serde/default/callback parity,
  pure helper dependencies, visibility, cfg boundaries and direct consumer
  imports. It did not independently execute the parent-run Cargo checks.
- The targeted nextest run passed **589 tests**, with five slow tests and
  3,198 tests outside the selected filter, in 522.973 seconds. It covered
  core contracts/execution, native journal and service recovery, managed
  storage, HTTP/WebSocket authority, provider continuity, SDK checkpoint
  exchange and maximum-tree completion after lost terminal ACKs. Source and
  manifests stayed unchanged throughout, including fixture subprocess builds.

The targeted runtime command selected only the required test binaries:

```sh
cargo nextest run -p bitrouter --all-features --lib \
  --test core_contract --test core_execution --test orchestrator_core \
  --test managed_native --build-jobs 2 --test-threads 2 \
  -E 'binary(core_contract) | binary(core_execution) | binary(orchestrator_core) | binary(managed_native) | test(core::) | test(harness::agent::) | test(harness::resources::managed::) | test(harness::service::) | test(managed_store::) | test(agent_store::)'
```

This stage removes the snapshot dependency on online scheduling; it does not
prove a harness-only feature profile. Host bridges still need explicit profile
wiring, and public harness exchange DTOs still need their separate contract/SDK
pass. The complete feature/startup matrix, historical writer fixtures,
terminal-free headless renderer, delivery/performance checks, full P0 review
and PR remain open. All final A01–A16 acceptance gates remain pending.

## Scheduler isolation and offline native inspection

Changes against `ab89178` put the live scheduler and managed HTTP/WebSocket
service behind `core`. The local native driver and managed-session adapter move
to `harness::agent::native::driver` and
`harness::resources::managed::store::local`, respectively. Token comparison of
both moved bodies against the parent passes after normalizing their new module
visibility and qualified paths. Shared journal decoding, immutable-history
validation and display projection remain available without core.

The core-disabled ThreadService retains a bounded, authorized read cache for
HTTP/IPC observation. It neither takes workspace fences nor starts queued native
work. An active observer pins its projection; after detachment the projection
can unload without resolving or rewriting stored recovery state. Shutdown skips
execution-owner/queue mutations in this profile. New native execution requests
fail before workspace discovery, resource initialization or authority acquisition.
HTTP, local IPC and ACP v1/v2 expose the unavailable execution capability; the
host does not initialize native execution or retain its lifetime handoff permit
when core is disabled. Existing external ACP paths remain usable.

The base gateway now detects managed Responses opt-ins even without the core
host wrapper. It rejects them explicitly and preserves ordinary request bytes.
The shared inspection retains the prior size, time and concurrent-read bounds;
cancelled reads release capacity. Enabling core continues to use the managed
adapter's existing protocol negotiation and authority checks.

Validation evidence:

- The core-disabled targeted suite passes **129 tests** (3,086 outside the
  filter), including real HTTP capability/auth/mutation/SSE behavior, local IPC
  observation/unload, negotiated ACP v1/v2 creation rejection, offline cache and
  shutdown behavior, external ACP, workspace tools and gateway routes. This is
  `--no-default-features --features harness,tui,mcp,otel-http`; it proves scheduler
  isolation, not the final minimum or terminal-free harness profile.
- Four journal replay tests use exact records from the real native driver in
  `tests/fixtures/native-replay/journal.json.zst`. They accept the recorded run
  and reject a corrupted digest, an earlier immutable fact rewritten under a
  valid digest, and inconsistent display projection. Fixture SHA-256 is
  `7970cc8eadadce2bdd933e7c10ba3661eaaa0db3f6a6f9b43c4b4c0fba610a5c`.
  Its producer is the current P0 writer after `ab89178`; this is not evidence for
  an older release's payload compatibility.
- Workspace all-feature/all-target and application core-disabled/all-target
  strict Clippy pass with Rust 1.97.0. Strict Rustdoc passes for the workspace
  with all features and the application with core disabled. Formatting and
  documentation-link checks pass. Independent read-only code review reports
  no findings in this stage's scope.
- Full-core/native regression passes **639 tests** across seven binaries in
  529.015 seconds, with six slow cases and 3,162 outside the selected/active
  tests. Coverage includes native scheduling and tools, ThreadService
  approval/recovery/queue/ownership, real-process HTTP/IPC and ACP v1/v2,
  managed HTTP/WebSocket and SDK checkpoint exchange, protocol conversion and
  full 32-agent recovery/capacity scenarios. Source and manifests stayed frozen
  throughout the run, including fixture subprocess builds.

The targeted runtime commands use separate feature invocations:

```sh
cargo nextest run -p bitrouter --no-default-features \
  --features harness,tui,mcp,otel-http --lib --build-jobs 2 --test-threads 2 \
  --no-fail-fast \
  -E 'test(gateway::http::) | test(harness::agent::native::replay_tests::) | test(core_disabled_tests::) | test(harness::context::) | test(harness::tools::) | (test(harness::acp::) & !test(harness::acp::native::)) | test(agent_local::tests::)'

cargo nextest run -p bitrouter --all-features --lib \
  --test core_contract --test core_execution --test orchestrator_core \
  --test managed_native --test native_acp --test native_agent_process \
  --build-jobs 2 --test-threads 2 --no-fail-fast \
  -E 'binary(core_contract) | binary(core_execution) | binary(orchestrator_core) | binary(managed_native) | binary(native_acp) | binary(native_agent_process) | test(core::) | test(harness::agent::) | test(harness::resources::managed::) | test(harness::service::) | test(managed_store::) | test(agent_store::) | test(gateway::http::) | test(agent_local::tests::)'
```

Strict lint uses `cargo +1.97.0 clippy --workspace --all-features --all-targets
--jobs 2 -- -D warnings`, and a separate `-p bitrouter --no-default-features
--features harness,tui,mcp,otel-http --all-targets --jobs 2 -- -D warnings`
invocation. Rustdoc uses the corresponding workspace/all-feature and
application/core-disabled selections with `--no-deps --jobs 2` and
`RUSTDOCFLAGS='-D warnings'`. Runtime tests use stable Rust 1.99.0. All use
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`; the 1.97 checks use a separate target directory.

The accepted plan now also distinguishes optional HTTP task activation from
local task/ACP listener construction, and limits the no-custom-compilation
promise to ordinary official-gateway use. The implementation plan retains its
verified legacy TUI retirement decisions and links baseline sources to their
pinned revision.

All final A01–A16 gates remain open. Serving-time workflow/policy and eval/outbox
separation, complete dependency/activation isolation, terminal-free headless
formatting, public harness contracts/clients, historical format/backend evidence,
packaged service delivery, performance comparison, final review and PR remain
required P0 work. P1 remains excluded.
