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
| P0 3 Gateway and lightweight AI | In progress | Old SDK service and AI model/provider runtime moved to application; AI contract isolation and current public API guards verified; serving workflow, observations, settlement and judge admission moved to gateway with shared durable records in storage; eval publication is optional; canonical inventory/observation/bound records and dispatch fences now have a gateway owner; evaluation, optimization, workflow research and policy production are core-owned; published block serving is gateway-owned and evolution learning is core-gated; final dependency matrix remains |
| P0 4 HTTP SDK | In progress | Typed gateway HTTP/SSE, control, eval and core HTTP/WebSocket clients verified; remaining harness API consumer extraction and independent packaged-service process consumer remain |
| P0 5 Feature isolation | In progress | Scheduler/learning and optional ingress gates are in place; supervisor startup and ACP route leases require harness while gateway identity/metering remain unconditional; minimum, core-only, harness-only and MCP-only ingress process tests pass; no-OTLP and disabled-core behavior verified; remaining harness/TUI module and dependency isolation precede the complete matrix |
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


## Exporter feature isolation

Changes against `d99f3d3` remove exporter-bearing assembly, host subscriber,
HTTP ingress, account adapter and ACP fields/calls when neither OTLP transport
is enabled. Application export configuration remains available without OTel;
request settlement, basic tracing and harness-local ACP records stay outside the
export gate. The observer status reports that export is not compiled in instead
of constructing a dummy exporter. Provider/account authentication remains
available independently of its optional export adapter.

The same effective opt-in parser checks first-party, nested OTLP and endpoint
environment activation. A selected unavailable exporter fails before its
installation-ID resolver or credential source runs. Dormant settings retain
their representation and existing precedence. File and zero-config startup,
reload, handoff preflight, standalone SessionHost and direct interactive/piped
chat apply the check. Enabled-exporter initialization retains its existing
failure policy and HTTP-versus-gRPC behavior.

Independent review identified two missing early checks in the first candidate:
the generated-default configuration branch and direct interactive chat dispatch.
Both were corrected. New tests exercise an environment-only zero-config process
and a real PTY calling the public chat entry with a supplied configuration. The
PTY test verifies terminal stdin/stdout and rejection before drawing or creating
session state. Existing full-stack auth, policy, request-check and streaming
metering tests remain enabled without OTLP; only export-specific cases/assertions
require their HTTP transport.

Validation evidence:

- No-OTLP targeted runtime: **60 passed**, 3,115 outside the selected filter,
  in 3.682 seconds. This uses `--no-default-features --features harness,tui,mcp`;
  it proves exporter isolation, not the final minimum or terminal-free harness
  profile. It covers actual serve/configuration/reload/preflight/PTY entry paths,
  standalone ACP preparation, external ACP lifecycle and ordinary request
  finalization. Two earlier runs exposed fixture assumptions about the existing
  stdout JSON error envelope and explicit `BITROUTER_HOME` resolution; corrected
  fixtures now exercise the intended branches.
- The no-OTLP normal/build dependency closure contains no OpenTelemetry,
  tracing-OpenTelemetry or tonic packages. Test-only OTLP protocol dependencies
  are not treated as production dependency evidence.
- Strict Rust 1.97.0 application no-OTLP/all-target and workspace
  all-feature/all-target Clippy passed. The final zero-config fixture correction
  also passed targeted strict Clippy. Strict workspace all-feature and application
  no-OTLP Rustdoc passed. Formatting and whitespace checks passed.
- HTTP-only regression: **116 passed**, 3,284 outside the filter, in 4.216
  seconds. It covers schema conformance, ingress/body lifetime, trace hierarchy,
  batching, metrics, HTTP bearer refresh, shared account auth, standalone ACP
  wiring, full-stack metering and real host/reload lifecycle.
- gRPC-only collector test: **1 passed**, no skips, in 0.029 seconds, verifying
  actual span/metric delivery, static metadata and shutdown flush. HTTP is absent
  from that build; HTTP-preferred all-feature tests are not used as a substitute.
- Independent review confirmed the early-validation fixes and corrected process
  fixtures. Final documentation review found no actionable issues. It reviewed
  source and reported evidence, without independently running Cargo.

The no-OTLP runtime command is:

```sh
cargo nextest run -p bitrouter --no-default-features \
  --features harness,tui,mcp --lib --test telemetry_disabled \
  --test configuration_state --test full_stack --test extension_host \
  --build-jobs 2 --test-threads 2 --no-fail-fast \
  -E 'test(assemble::telemetry_opt_in_tests::) | test(assemble::otel_config_tests::) | test(telemetry::config::) | test(cloud::tests::) | test(acp_cli::session_host_tests::) | test(harness::acp::capture::) | test(harness::acp::telemetry::) | binary(telemetry_disabled) | binary(configuration_state) | binary(full_stack) | binary(extension_host)'
```

The transport regression commands are:

```sh
cargo nextest run -p bitrouter --no-default-features \
  --features core,harness,tui,mcp,guardrails,otel-http --lib \
  --test full_stack --test observe_hierarchy --test telemetry_ingress_log_target \
  --test configuration_state --test extension_host \
  --build-jobs 2 --test-threads 2 --no-fail-fast \
  -E 'test(telemetry::) | test(assemble::telemetry_opt_in_tests::) | test(assemble::otel_config_tests::) | test(cloud::tests::) | test(acp_cli::standalone_cloud_credentials_tests::) | test(acp_cli::session_host_tests::) | binary(full_stack) | binary(observe_hierarchy) | binary(telemetry_ingress_log_target) | binary(configuration_state) | binary(extension_host)'

cargo nextest run -p bitrouter --no-default-features \
  --features core,harness,tui,mcp,otel-grpc --test telemetry_grpc_transport \
  --build-jobs 2 --test-threads 2 --no-fail-fast
```

Runtime processes explicitly unset ambient `OTEL_EXPORTER_OTLP_ENDPOINT`;
opt-in subprocess fixtures set their own isolated environment. The Cargo/Rust
versions, debug-symbol/incremental settings and separate strict-check target
directory remain those recorded above. Source and manifests stay frozen during
runtime tests and any nested fixture builds.

The accepted plan's added exporter/configuration split and explicit harness HTTP
SDK boundary are synchronized here without undoing reviewed TUI retirements or
implementation status. All final A01–A16 gates remain open. Feature combinations
that omit harness/TUI/MCP, gateway serving-policy versus core-learning separation,
public harness clients, historical/back-end evidence, packaged-service delivery,
performance, full final verification and PR are still required. P1 is excluded.


## Serving workflow and experiment ownership

Changes against `2e46471` move request-time workflow projection into
`gateway::routing::workflow`: `decision`, `extractors` and its seven adapters,
`ir`, `online`, `predictive` and `session`. Published experiment data and stable
assignment move from `optimization::exploration` to gateway routing;
`optimization::identity` moves to `gateway::routing::experiment_identity`.
Policy loading/routing, trajectory guards, session attribution, offline analysis,
CLI reports and integration fixtures now import their actual gateway owner.
There are no forwarding modules or duplicated conversion/projection algorithms.

All 15 relocated source files, including their existing tests, match the parent
revision exactly after the enumerated module-path and fixture-relative-path
changes followed by the same formatter. This preserves scorecard constants,
experiment digest domains, assignment behavior, serialized records and existing
fixture contents. Mixed evaluation observers and research/replay remain in their
current module until their consumer boundary is separated. The only references
from the moved subtree to those owners are existing test helpers; production
projections and experiment validation have no core, harness or eval import.

This is an ownership step toward P0 3, not completed core feature isolation.
`policy_table_router` still consumes pending evaluation decisions and measurement
helpers. Policy compilation/evidence inspection, the gateway settlement adapter,
reserved-request protections, durable outbox consumption and learning startup
still need the planned split. The same applies to the live ACP adapter in session
identity. No minimum-gateway runtime claim follows from this relocation.

Validation evidence:

- Stable application all-feature/all-target `cargo check` passed.
- Full-feature focused runtime: **482 passed**, 3,001 outside the selected filter,
  in 9.990 seconds across the library and four integration binaries.
- Core/OTLP-disabled focused runtime: **482 passed**, 2,773 outside the filter,
  in 8.197 seconds. This profile still enables `harness,tui,mcp`; learning
  services are not yet gated, so it does not prove the final gateway-only build.
- Both runs cover online projection, predictor contracts, deterministic experiment
  assignment, policy/certificate validation, route decisions, session attribution,
  trajectory guards/health, replay and reward/report consumers. The trajectory
  process integration retains continuation authority and durable guard behavior.
- Rust 1.97.0 strict workspace all-feature/all-target Clippy and application
  core/OTLP-disabled all-target Clippy passed. Strict Rustdoc passed for both
  the all-feature workspace and the disabled application profile.
- Independent read-only review found no source correctness blockers. It found
  three relative source links broken by relocation; the two extractor links now
  point to their new owners, and the historical core audit link is pinned to its
  documented baseline commit. Final link/fmt/whitespace checks are recorded with
  the reviewed candidate.

Runtime commands use the same filter in separate feature invocations:

```sh
serving_filter='test(gateway::routing::workflow::) | test(gateway::routing::exploration::) | test(policy_lock::) | test(policy_table_router::) | test(policy_compile::) | test(optimization::) | test(session_identity::) | test(workflow_state::) | test(trajectory::guard::) | test(trajectory::health::) | binary(workflow_state_replay) | binary(terminus_2_workflow_state) | binary(smithers_reward_loop) | binary(trajectory_progress_control)'

cargo nextest run -p bitrouter --all-features --lib \
  --test workflow_state_replay --test terminus_2_workflow_state \
  --test smithers_reward_loop --test trajectory_progress_control \
  --build-jobs 2 --test-threads 2 --no-fail-fast -E "$serving_filter"

cargo nextest run -p bitrouter --no-default-features \
  --features harness,tui,mcp --lib \
  --test workflow_state_replay --test terminus_2_workflow_state \
  --test smithers_reward_loop --test trajectory_progress_control \
  --build-jobs 2 --test-threads 2 --no-fail-fast -E "$serving_filter"
```

The stable/strict toolchains, separate lint target and disabled debug/incremental
settings remain as recorded above. Runtime processes unset ambient OTLP endpoint
configuration. Source and manifests stayed frozen throughout both runs, including
nested fixture builds. The plan's latest concrete contract/interface split points, harness HTTP
DTO scope and separate P1 acceptance IDs are synchronized without changing the
implementation status or reviewed legacy TUI retirements. P1 is not implemented.


All final A01–A16 gates remain open. The next ownership work is the gateway's
pending decision/settlement contract and published-policy services versus core
learning/evaluation, followed by the remaining feature/startup and delivery gates.
This stage has not created a PR or published an artifact.

## Gateway settlement and optional evaluation consumption

Changes against `2ac9664` move pending policy decisions, bounded prediction
observations and routing-evidence construction into gateway observability.
The response observer moves from `workflow_state::response_observer` to
`gateway::observability::prediction`; the new gateway settlement recorder owns
trajectory disposition and delegates ordinary request evaluation only when a
consumer is assembled. Shared persisted evidence validation and digests move
from `eval::types` to `storage::evaluation`. Classifier cohort construction stays
with the evaluation compiler, not the shared storage contract.

The response observer is byte-equivalent after its import changes and rustfmt.
The evidence validator is likewise equivalent after its module documentation,
self-reference updates and the unchanged classifier function's move; all 14 of
its tests remain. Public evaluation DTOs stay in AI. No service algorithm or
new trait/framework was added to the public contract library.

Gateway trajectory settlement still runs after authoritative metering. It owns
the existing atomic terminal fact/outbox persistence and accepts an optional
notification callback. The production publisher, startup/shutdown drain and
`/v1/evals/*` route mounting now require core. Without core, no evaluation service
or automatic publisher is assembled, and pending outbox rows remain durable,
inspectable and protected from retention. Unit-test publisher fixtures remain
available independently to preserve their durability/validation coverage.

Independent review found a concurrency regression in the first split: the
ordinary eval delegate re-read pending state after an awaited trajectory lookup.
Cache eviction or expiry could discard the original decision. The fix carries
the captured decision through a private, fully redacted event bound to its
invocation and owner, including an explicit absent snapshot. Retries select the
latest capture; cleanup still follows successful persistence. Tests hold the
single SQLite connection to suspend the actual trajectory lookup, then evict the
original entry or insert a late entry. They verify that the original decision is
retained and a late decision is not substituted. A separate test checks owner,
invocation and serialization isolation. These three regressions passed before
running the broader final candidate checks.

Validation evidence for the reviewed candidate:

- Full-feature focused runtime: **407 passed**, 3,023 outside the selected filter,
  in 12.145 seconds.
- Core/OTLP-disabled focused runtime: **403 passed**, 2,795 outside the filter,
  in 10.035 seconds, with `--no-default-features --features harness,tui,mcp`.
  This remains an intermediate profile, not the final minimum gateway.
- Both runs retain response-observation, evidence validation, ordinary eval,
  policy compilation/routing, trajectory disposition, idempotent settlement,
  publisher/restart, progress-guard, continuation and full-stack request tests.
  Existing core-only admission cases remain exercised in the full run.
- Added core-disabled assembly tests verify cleanup of untracked observations
  without eval subject/result writes. The trajectory HTTP test preserves one
  pending envelope and one terminal fact across retry, process-state restart and
  retention, with zero publication attempts and no repeated metering.
  The real CLI host test verifies eval routes return 200 with core and 404
  without it, while retaining ordinary inference and shutdown assertions.
- Rust 1.97.0 strict workspace all-feature/all-target Clippy and application
  core/OTLP-disabled all-target Clippy passed. Strict Rustdoc passed for the
  all-feature workspace and the disabled application profile. Formatting,
  whitespace, moved-source parity and updated documentation link checks passed.
- Final independent read-only source review reported no actionable findings,
  including the snapshot fix, missing-metering behavior, transaction/notification
  ordering, ownership and core-disabled wiring. The reviewer did not run Cargo;
  the runtime evidence above is recorded separately.

The final runtime commands share this filter:

```sh
settlement_filter='test(gateway::observability::decision::) | test(gateway::observability::prediction::) | test(storage::evaluation::) | test(trajectory::) | test(eval::) | test(assemble::trajectory_assembly_tests::) | test(policy_table_router::) | test(policy_lock::) | test(policy_compile::) | binary(trajectory_progress_control) | binary(policy_eval_control_plane) | binary(eval_exchange_equivalence) | binary(extension_host) | binary(full_stack)'

cargo nextest run -p bitrouter --all-features --lib \
  --test trajectory_progress_control --test policy_eval_control_plane \
  --test eval_exchange_equivalence --test extension_host --test full_stack \
  --build-jobs 2 --test-threads 2 --no-fail-fast -E "$settlement_filter"

cargo nextest run -p bitrouter --no-default-features \
  --features harness,tui,mcp --lib \
  --test trajectory_progress_control --test policy_eval_control_plane \
  --test eval_exchange_equivalence --test extension_host --test full_stack \
  --build-jobs 2 --test-threads 2 --no-fail-fast -E "$settlement_filter"
```

Toolchain/debug/incremental settings remain those recorded above. Ambient OTLP
endpoint configuration was unset for runtime invocations. Source and manifests
stayed frozen through the tests and nested CLI fixture builds. The accepted
plan's source-inventory table and two reviewed feature/delivery clarifications
are synchronized, with baseline links pinned to the inventory revision and the
implementation status/legacy TUI retirements preserved.

All A01–A16 final gates remain open. This stage does not gate all eval/learning
source or CLI operations: evolution startup and reserved-request protection
still require their gateway/core split. Full-to-minimum-to-full artifact
transition evidence, final dependency/startup isolation, harness HTTP clients,
packaging, performance, full final verification and PR remain required. P1 is
excluded. No artifact was published and no PR was created in this stage.

## Gateway request reservations and shared record ownership

Changes against `12dbe69` move operational judge admission, observation and
settlement to `gateway::metering::judge`. Assembly still registers that same
concrete hook before authentication for both generation and Decisions. Existing
job reservation construction and cost-report operations remain learning-owned
in `evolution::costs`; they extend the gateway-owned handle without moving job
scheduling or evaluation into the gateway. Replay rejection markers move to
`gateway::metering::replay`, so always-run metering and settlement no longer
import their marker types from the evolution runtime.

The actual shared durable dependencies move with this boundary:

- `EvolutionStore` and its unchanged digest algorithm to `storage::evolution`;
- persisted `ExecutionHop` and `ExecutionOutcome` to `storage::execution`;
- `SessionIdentity` and its existing storage index to `storage::session_identity`.

Capture, checkpoint cleanup, learning, CLI, examples and test consumers import
the new owners directly. No forwarding modules, duplicate record definitions,
new AI contracts or schema migrations are introduced. The gateway hooks and
shared records have no source import of evolution, ACP capture or the harness.
This does not yet gate evolution startup or canonical execution admission; the
existing request behavior remains active while that remaining split is prepared.

Source comparison verified the complete identity and execution definitions,
store and digest implementation, job reservation body, admission/finish path,
pre-request hook, observation/settlement implementations and event names against
the parent revision. Persisted table names, digest domains, field order/serde
representation and existing tests are preserved. No newly written version-tag
fixture is treated as historical data evidence.

Validation evidence:

- Stable all-feature/all-target application `cargo check` passed. The initial
  pass found one grouped test import still pointing at the old store and two
  imports now used only by relocated code/tests; those were corrected.
- Full-feature focused runtime: **216 passed**, 3,180 outside the filter,
  in 9.344 seconds. Core/OTLP-disabled runtime: **214 passed**, 2,950 outside the
  filter, in 9.115 seconds, with `--no-default-features --features harness,tui,mcp`.
  The only identity difference is the two expected OTLP full-stack cases.
- Coverage includes real pipeline judge admission and missing/unknown usage,
  cached responses, retries and authoritative receipts; canonical accounting,
  streaming/disconnect and authenticated capture scopes; atomic reservations,
  checkpoint deletion/recovery, policy learning and cost-report consumers; and
  CLI checkpoint/revision persistence across processes.
- Strict Clippy initially found a relocated test module before production impls.
  The unchanged module moved to the file end. After that layout-only repair,
  the eight judge accounting/reservation regressions passed again in both
  profiles: 0.361 seconds full, 0.293 seconds disabled. No production algorithm
  or test assertion changed to satisfy the lint.
- Final Rust 1.97.0 strict workspace all-feature/all-target Clippy and application
  disabled-profile all-target Clippy passed. Strict Rustdoc passed for the
  all-feature workspace and disabled application profile. Formatting, whitespace,
  source parity and updated documentation link checks passed.
- Independent read-only review found no actionable correctness or ownership
  findings. It checked persisted layouts/indexes, admission ordering, accounting,
  retained tests and nonlocal reservation/report implementations. It did not run
  Cargo; the test evidence is recorded separately.

The broader runtime commands were:

```sh
reservation_filter='test(evolution::) | test(gateway::metering::) | test(acp_trajectory::) | test(metering::) | test(eval::settlement::) | test(actions::code::evolution::) | binary(acp_checkpoints) | binary(full_stack) | binary(source_boundaries)'

cargo nextest run -p bitrouter --all-features --lib \
  --test acp_checkpoints --test full_stack --test source_boundaries \
  --build-jobs 2 --test-threads 2 --no-fail-fast -E "$reservation_filter"

cargo nextest run -p bitrouter --no-default-features \
  --features harness,tui,mcp --lib \
  --test acp_checkpoints --test full_stack --test source_boundaries \
  --build-jobs 2 --test-threads 2 --no-fail-fast -E "$reservation_filter"
```

The final layout verification used `--lib` in the same two profiles with
`test(gateway::metering::judge::) | test(evolution::costs::) |
test(evolution::runtime::tests::judge_costs::)`. Toolchain/debug/incremental
settings are unchanged; ambient OTLP endpoint configuration was unset. Source
and manifests stayed frozen during each runtime pass and its nested CLI builds.

This stage preserves existing behavior while correcting implementation ownership.
Evolution is still compiled and assembled in the disabled-core test profile;
those tests do not prove final minimum-gateway isolation. Remaining work includes
canonical gateway inventory/admission and their persisted capture dependencies,
learning activation/CLI gates, the rest of the feature matrix and all final P0
acceptance/delivery evidence. Full/minimum/full artifact transitions are still
unverified. All A01–A16 final gates remain open; no PR or publication is claimed,
and P1 remains excluded.

## Gateway canonical request inventory and tracking

Changes against `2f3ca1e` move `evolution::inventory` to
`gateway::observability::inventory` and extract `GatewayRequestTracker` into
`gateway::observability::requests`. The tracker owns the original authenticated
identity lookup, execution/inventory duplicate checks, request admission and
hop/terminal/settlement observations. `EvolutionRuntime` consumes the same typed
observation for optional learning selection and retains its existing routing,
session and control fences. Assembly installs the tracker immediately before
learning at router preparation, preserving generation-only operation scope and
the existing observer/settlement order. Control registration receives the
inventory directly from the gateway handle, preserving its serving runtime epoch.

The remaining shared durable definitions move to their actual application owner:

- `ExecutionRecord`, `ExecutionSettlement`, `EXECUTION_KIND`, `DecisionContext`
  and `RouteIntent` join `storage::execution`;
- capture connection/session/event ORM definitions move to `storage::capture`.

Gateway, checkpoint, learning, CLI and fixture consumers import these owners
without forwarding facades. The capture recorder, conversation projection and
checkpoint writers retain their existing authority. No new AI API, migration,
serialized representation or broad idempotency contract is introduced. Source
comparison verified the inventory implementation, capture ORM definitions,
identity lookup, execution/intent data, observation/settlement algorithms and
learning selection/fences against the parent revision. Moving an observation
into the gateway event avoids an extra clone; its existing event name is retained.

Independent review found that the new preparation hook initially inherited the
trait's rejecting `revalidate_context` default. The existing real-assembly
`orchestrator_core::assembled_app_revalidates_rebuilt_context_before_fresh_count_and_execution`
test reproduced that failure before repair: the second managed run failed with
`preparation hook has no context revalidation contract`. The tracker now explicitly
retains its original observation during authorized history removal without
re-admitting, writing or executing a request. Other hooks still revalidate their
live policy fences. The new focused test trims managed messages and verifies
that repeated revalidation preserves the original request/attempt and one
inventory entry. The existing assembly reconstruction test remains unchanged.

New real-HTTP pipeline fixtures construct no learning service, scheduler or
capture recorder. They seed the existing ORM representation to exercise tracked
and unresolved requests, duplicate rejection, protection of earlier execution
records, route-phase failures and ordinary preparation failures. Successful
execution is followed by the actual pending-work drain before asserting terminal
observation. Preparation failures deliberately retain unsettled inventory and
no usage row; route failures retain the existing pre-dispatch failed settlement.
The fixtures do not claim historical payload compatibility or full artifact
profile-transition evidence.

Validation evidence for this stage:

- Initial all-feature/all-target application check found grouped imports and
  inventory-handle callers still using the old owner; after updating them, the
  check passed in 46.50 seconds.
- The first new fixture pass exposed two incorrect test assumptions: ordinary
  preparation rejection does not settle, and successful delivery observation
  completes asynchronously. Tests now preserve those existing semantics and
  use the actual pipeline drain. The four original gateway fixtures then passed.
- Independent review's context-revalidation regression was reproduced with the
  unchanged real-assembly test before repair. The final broad full-profile run
  passed **281/281** in 84.903 seconds, with 3,186 outside the filter. It includes
  all five gateway-only fixtures and the entire `orchestrator_core` integration
  suite, including real process recovery, managed history reconstruction,
  authentication, continuations and protocol/accounting behavior.
- Final Rust 1.97.0 strict all-feature/all-target workspace Clippy and disabled
  application all-target Clippy passed. Strict Rustdoc passed in both profiles.
  Formatting, whitespace and source-parity checks passed.
- Disabled-core/OTLP runtime passed **213/213** in 8.695 seconds, with 2,956
  outside the filter. Its profile is `--no-default-features --features
  harness,tui,mcp`. All five independent tracker regressions passed in both profiles.
  The full run has exactly 66 additional orchestrator integration cases and two
  existing OTLP full-stack cases; there is no unexplained test-identity gap.
- Independent follow-up review confirmed the context-revalidation finding is
  resolved and found no additional actionable issue. Review was read-only;
  Cargo evidence is recorded separately.

The final runtime commands use stable, `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, and two build/test jobs:

```sh
inventory_filter='test(evolution::) | test(gateway::observability::requests::) | test(gateway::metering::) | test(acp_trajectory::) | test(metering::) | test(actions::code::evolution::) | binary(acp_checkpoints) | binary(full_stack) | binary(source_boundaries) | binary(orchestrator_core)'

cargo nextest run -p bitrouter --all-features --lib \
  --test acp_checkpoints --test full_stack --test source_boundaries \
  --test orchestrator_core --build-jobs 2 --test-threads 2 --no-fail-fast \
  -E "$inventory_filter"

cargo nextest run -p bitrouter --no-default-features \
  --features harness,tui,mcp --lib \
  --test acp_checkpoints --test full_stack --test source_boundaries \
  --build-jobs 2 --test-threads 2 --no-fail-fast -E "$inventory_filter"
```

The before-repair reproduction selected only
`assembled_app_revalidates_rebuilt_context_before_fresh_count_and_execution` from
`--test orchestrator_core --all-features`. Strict checks use the same commands
and separate 1.97 target directory documented in preceding stages. Source and
manifests remained frozen during runtime tests and their nested executable builds.

All final A01–A16 gates remain open. Bound `ExecutionRecord` initialization and
its routing/session/control fences still run in `EvolutionRuntime`. Before
removing that runtime from a profile, preserve any gateway-required record
creation, cross-controller canonical identity protection and live revalidation;
the tracker-only fixtures do not establish that complete transition. Evolution
source/startup/commands are still present in the core-disabled profile; passing
bookkeeping tests do not establish minimum-gateway isolation. Remaining work includes learning feature gates, the other package/module
and feature boundaries, historical/backend/artifact/performance evidence, full
validation, final review and PR. P1 and package publication remain excluded.

## Gateway execution binding and dispatch fences

This stage follows `0e62fbd`. The gateway now owns bound execution initialization
and the routing/policy-generation and captured-session checks that previously
remained in `EvolutionRuntime`. `gateway::observability::binding` contains the
internal `CanonicalRequestBinding`; it runs after the optional learning selection
at router preparation and before the learning control fence during route
resolution. It also explicitly revalidates managed context changes without
re-admitting or rewriting the request.

`CanonicalSelection` carries the exact routing generation and policy snapshot
used by the learner, together with its optional persisted intent and effective
route. It is internal application context, without a live core handle or a new
AI contract. With no learner installed, binding uses the current gateway selector
and snapshots, creates the same execution representation with no intent, and
retains coverage diagnostics. Learning still owns route selection and live
control-generation checks. The bound event name, persisted representation,
owner-scoped initialization, conflicting-attempt marker, comparison order and
ordinary finalization semantics are retained.

The standalone request fixture now uses the real configurable routing table,
policy runtime, HTTP executor, metering recorder, tracker and binding. It never
constructs `EvolutionRuntime`, a learning scheduler, a capture recorder or a
harness session service. A bound request creates one execution record whose
attempt and settlement agree with inventory; an unresolved request retains
inventory only. The fixture checks that no control, enrollment or route-intent
records appear.

Additional regression coverage exercises:

- a second authenticated-controller fixture scope reusing an owner-scoped
  execution ID after completion, with one upstream dispatch and the original
  charge/record preserved;
- two inventory admissions completing before either initializes the execution,
  deterministically exercising the competing-attempt window without relying on
  task timing; only the first attempt acquires the bound record;
- routing reload, policy reload and session deletion after admission, rejecting
  dispatch with the existing failed pre-dispatch settlement;
- the same three changes after authorized history removal, with both preparation
  and route revalidation rejecting and leaving execution/inventory unchanged;
- routing or policy reload between learning selection and bound-record creation,
  proving that binding retains the earlier selection snapshots rather than
  refreshing the fence to admit stale work.

The first focused runtime run passed six of eight tests. Both failures were in
new second-controller fixture setup: its event reused the existing unique
session sequence. The fixture now appends sequence 2 and advances the shared
session head, preserving the actual schema constraint. No production check or
asserted behavior was relaxed. The final focused run passed **10/10** in
0.291 seconds, including nine independent gateway cases and the learning-to-
binding snapshot case.

Independent review found no implementation defect and identified two missing
regressions: stale context revalidation and reload during the selection/binding
handoff. The added cases above address both. Review is read-only and is separate
from runtime evidence. Independent follow-up confirmed both gaps are resolved,
with no remaining source-review finding or documentation overclaim for this stage.

Validation already completed on the changed source:

- All-feature/all-target application compilation passed.
- Rust 1.97.0 strict all-feature/all-target workspace Clippy and core/OTLP-disabled
  application all-target Clippy passed after the final test additions.
- Strict Rustdoc passed with all features and with `--no-default-features
  --features harness,tui,mcp`. Subsequent changes only added or repaired test code.
- Formatting and patch-whitespace checks passed.

Broader runtime validation completed with **286/286** passing in 65.294 seconds
for the all-feature profile and **218/218** passing in 9.016 seconds for
`--no-default-features --features harness,tui,mcp`. Respectively, 3,186 and 2,956
tests were outside the selected filter. Test-identity comparison found exactly
66 core integration cases and two OTLP full-stack cases present only in the full
run, with no unexplained gap. All nine independent gateway regressions and the
learning snapshot-handoff case ran in both profiles. Source/manifests remained
frozen during these tests and their nested executable builds. The runtime
commands and filter are the same as the preceding stage; the new cases are
included by their owning module paths. Logs use the
`/tmp/bitrouter-p0-binding-*` prefix.

The stable runtime build retained the previously recorded large unwind-section
linker warning; the strict Rust 1.97.0 checks passed. The third-party
`proc-macro-error2` future-compatibility notice remains unchanged.

All final A01–A16 gates remain open. This extraction removes the remaining generic
canonical record/fence dependency on the learner, but evolution source, startup
and commands still compile in the core-disabled profile. It does not establish
the minimum-gateway closure. Learning feature gates, the remaining ownership and
feature work, historical/backend/artifact/performance evidence, full validation,
final review and PR remain required. P1 and package publication remain excluded.

## Core learning, evaluation and policy production

The application now owns adequacy analysis, evaluation, optimization, workflow
research and policy production under `core`. Policy-file compilation, evidence
verification, evolution and candidate publication moved from `policy_lock` to
`core::policy::files`; published-policy loading, validation, inspection, reload
and rollback remain usable by the gateway. Evaluation outbox consumption moved
to `core::eval::publisher`. Gateway terminal facts, envelope construction,
accounting and pending-outbox inspection remain unconditional.

The optional pending-decision consumer and workflow-trace recorder are compiled
only with core. The gateway settlement adapter still performs authoritative
terminal handling and releases its bounded process-local pending state without
that consumer. CLI learning operations reject unavailable core support before
opening files or stores. Nonempty `BITROUTER_WORKFLOW_TRACE_JSONL` is rejected
by `serve`, `start` and `restart` before launcher mutations or resident shutdown.

Independent review found two gaps: the original trace guard ran too late for
`start`/`restart`, and gating the entire workflow-replay target hid four gateway
regressions. The guard now runs at each entry point. A real isolated resident
process test verifies a rejected restart leaves that resident running. The four
unchanged gateway test bodies now live in ungated `gateway_workflow`, sharing
fixtures with the 56 core replay tests. CI includes both targets. Follow-up
review identified two unused imports, which were removed; final source review
closed with no remaining finding.

Validation on this stage:

- The broad all-feature selection passed **886/886** in 68.311 seconds; 2,755
  cases were outside its filter. Existing live external-agent cases remained
  ignored; this does not establish live-provider evidence.
- The corresponding core/OTLP-disabled selection (`--no-default-features
  --features harness,tui,mcp`) passed 610 of 611 cases. The sole failure was a
  new zero-config fixture selecting an explicit home without a config. After
  correcting that fixture, both process cases passed; no production check or
  assertion was relaxed.
- After review fixes, the disabled selection passed **7/7** in 2.537 seconds
  (three process cases and four gateway workflow cases). The affected full
  selection passed **122/122** in 8.858 seconds, with 3,403 cases filtered out.
- Strict Rust 1.97.0 workspace/all-feature/all-target Clippy passed after the
  final unused-import removal. All-feature strict Rustdoc passed. Final
  core/OTLP-disabled strict Clippy and Rustdoc also passed for the profile above.
- Formatting and patch-whitespace checks passed. Source/manifests were frozen
  during runtime tests and their nested executable builds. Logs use the
  `/tmp/bitrouter-p0-learning-*` prefix.

An actual `--no-default-features` probe failed before running tests with nine
compiler errors in remaining MCP assembly/tool references and the harness native
replay-close path. This is explicit incomplete feature-matrix evidence, not a
passing minimum-gateway claim. Evolution startup/source, harness/TUI/MCP and
remaining dependency isolation still require implementation. All final A01–A16
gates remain open; historical/backend/artifact/performance evidence, complete
validation, final independent review and PR remain required. P1 is excluded.


## Core evolution and published block serving

This stage follows `d0db976`. Evolution posterior inference, scoring, jobs,
operator mutations, publication and workers now live under `core::evolution`.
The core feature owns `rand_distr`; the core-disabled normal/build dependency
tree has no such dependency. CLI, IPC, TUI actions and examples follow their
owner, while existing stored data and gateway behavior remain independently
available.

Independent review caught an incorrect initial boundary: gating the entire
old evolution hook would stop applying already adopted blocks. The corrected
`gateway::routing::blocks` owns published allocation/control data, baseline
ancestry and dependency checks, sticky enrollment and transactional route intent,
serving catalog checks and the request-time admission/revalidation fences.
`BlockPolicyRouter` is installed unconditionally in the original hook order.
Core owns the learning and operator methods on that state. Feature selection
does not rewrite the persisted evolution mode.

The three existing capture DTOs moved unchanged to `storage::capture`, removing
the policy store's dependency on a live harness capture module. The writer and
controller remain harness-owned. An audit compared 15 original serving method
bodies against their extracted implementations, normalizing only whitespace,
owner/module qualifiers and visibility: all matched. Independent review also
confirmed unchanged compared serde representations and retention of all 87
original evolution tests.

Three new ungated gateway regressions dispatch through an actual mock upstream:
persisted adopted blocks still choose the candidate after reopening a disk
store (including named policies), preexisting baseline sessions retain their
membership, and changed route dependencies trigger the original fallback.
The persisted-adoption case also checks duplicate rejection and unchanged
control state. These are controlled current-schema fixtures, not empirical
promotion evidence or historical/cross-artifact compatibility proof.

The initial core-disabled runtime selection passed 401 of 402 tests. The new
unavailable-command test found session-log creation before rejection. The guard
now runs before logging or file/store access; 14 unavailable command forms are
covered without weakening an assertion. Review also found a pure fresh-session
PTY regression hidden inside the core-only evolution target. Its unchanged body
and supporting enum now live in the ungated parent suite. Follow-up independent
review reported no remaining actionable source finding.

Validation on the corrected stage:

- All-feature application/all-target checking passed. The selected full runtime
  suite passed **546/546** in 84.418 seconds (two slow; 3,022 filtered out),
  covering evolution, capture/controller, gateway accounting/routing, daemon,
  full-stack, checkpoints, CLI and the affected PTY behavior.
- The core/OTLP-disabled profile (`--no-default-features --features
  harness,tui,mcp`) passed **407/407** in 80.184 seconds (one slow; 2,677
  filtered out), including all new block-routing cases, unavailable CLI/IPC
  behavior and the ungated fresh-session PTY case.
- After making the posterior dependency optional and updating example
  provenance, the final disabled block/CLI/daemon selection passed **22/22**
  in 2.934 seconds, with 2,932 filtered out.
- Final strict Rust 1.97.0 workspace/all-feature/all-target Clippy and disabled
  application/all-target Clippy passed. Strict Rustdoc passed for the full
  workspace and disabled application; later source changes affected only
  example provenance and the optional dependency declaration.
- Formatting and patch-whitespace checks passed. Runtime source/manifests were
  frozen during tests and nested example builds. Logs use the
  `/tmp/bitrouter-p0-block-routing-*` prefix. The previously recorded stable
  linker and third-party future-compatibility warnings remain unchanged.

The final plan also incorporates the documentation review's explicit published
block migration, persisted-mode and cross-profile acceptance requirements.
All A01–A16 gates remain open. Empty-feature MCP/harness errors, remaining
module/dependency/startup isolation, harness SDK coverage, historical/backend/
artifact/performance evidence, whole-candidate validation and independent final
review still precede the requested PR. P1 and publication remain excluded.

## Optional MCP and harness ingress

This stage follows `8da53d4`. MCP executor/routing construction and diagnostics
now require `mcp`. Local task/ACP listeners, ThreadService resources and the
optional execution HTTP listener require `harness`; their existing lifecycle
is retained when enabled. Native managed CLI execution requires both core and
harness. Unsupported commands reject before configuration discovery or logging.
Configured MCP servers and enabled execution HTTP reject at assembly, reload
and profile preflight when their capability is absent.

The new isolated-process suite exercises real gateway inference and shutdown.
Without harness it supplies dormant invalid workspace paths and sentinel files
at task/ACP endpoint paths, then verifies those resources remain untouched.
With harness it connects to both existing local endpoints. It also checks
unsupported configuration and CLI forms for side effects. The original process
fixture used an incorrect provider route and model ID; correcting its fixture
and retaining explicit upstream-call counting produced passing tests without
relaxing production validation. Independent review found no actionable source
finding in the implementation, including the early native-task guard.

Validation:

- Actual empty-feature application/all-target checking passed; this resolves the
  nine compiler errors recorded by the preceding stages, not the remaining
  dependency or runtime isolation gates.
- The full-feature affected runtime selection passed **431/431** in 17.188
  seconds, with 3,095 tests outside the filter. It covers host/assembly,
  local clients, MCP, native ACP, configuration, extensions, daemon and HTTP.
- Latest-source isolated profile runs passed: minimum **3/3** (2.136 seconds),
  core-only **3/3** (2.439 seconds), harness-only **10/10** (2.551 seconds;
  2,931 filtered out), and MCP-only **5/5** (2.259 seconds). The latter two
  retain the existing MCP discovery and protocol-version tests.
- Strict Rust 1.97.0 full-workspace/all-feature/all-target and empty-feature
  application/all-target Clippy passed. Strict Rustdoc passed for both profiles;
  optional MCP/native documentation links were corrected for reduced builds.
- Formatting and patch-whitespace checks passed. Source/manifests remained
  frozen throughout runtime tests, including sequential profile builds. Logs
  use `/tmp/bitrouter-p0-optional-ingress-*`. The previously recorded toolchain
  linker and third-party future-compatibility warnings remain unchanged.

All final A01–A16 gates remain open. The minimum host still constructs an ACP
route-lease adapter and opens the supervisor ledger; full harness/TUI dependency
isolation, server-tool/database flags and the remaining artifact, compatibility,
performance, complete validation and final review work are outstanding. This
stage does not establish complete gateway runtime isolation or P0 delivery.

## Harness supervisor and route-lease runtime isolation

This stage follows `62321c2`. The foreground daemon constructs, opens and shuts
down the supervisor only with harness. Without that feature, local supervised
session and ACP route-lease commands retain their wire representation but return
an explicit capability error; handoff status reports zero local supervised runs
and leases. Harness-enabled custom hosts without a supervisor still report
unknown activity. Persisted recording checks remain conservative and available;
feature disablement does not turn existing recording rows into proof of idle
activity.

The ACP route runtime moved byte-for-byte from `src/acp_runtime.rs` to
`src/harness/acp/runtime.rs`, behind harness, with no root forwarding facade.
Assembly and control wiring no longer construct it in gateway/core-only builds.
`SessionContextHook` remains a required, unconditional gateway hook: its default
normalizes request identity without a route-lease adapter. The full host still
injects the shared principal-scoped runtime in the original hook order. Existing
session-attributed metering and control reads remain usable without harness.

The process fixture now seeds a malformed supervisor ledger at its persisted
v1 path before starting a reduced gateway. It proves startup, real inference,
status, unavailable IPC rejection and shutdown succeed without decoding or
rewriting that file or creating its temporary replacement. This is a dormant
resource-isolation test, not a historical schema compatibility claim.

Independent review identified mixed identity tests whose initial harness gates
also hid ordinary Claude/Codex lineage, turn and conflict assertions. Both tests
now run without harness; only lease setup/assertions are conditional. A new
unconditional case checks declared controller/native attribution without a lease
adapter. All eight original identity cases remain, plus that ninth case. Core
mode-change fencing and generic capture/session-spend tests remain ungated.
Follow-up review confirmed the correction and unchanged runtime relocation with
no remaining actionable finding.

Validation uses debug-symbol-free, non-incremental builds, two build jobs and
two test threads as in preceding stages:

- Empty-feature and core-only application/all-target checks passed. Initial
  fixture compilation exposed a private helper reference and one unused import;
  the fixture now uses the documented persisted filename encoding, and the
  import follows its runtime owner.
- Before the final test-gate correction/runtime relocation, affected nextest
  suites passed: minimum **33/33** in 2.068 seconds; all features **137/137** in
  6.793 seconds; core-only **107/107** in 6.324 seconds; harness-only **40/40**
  in 2.969 seconds. These include daemon, process ingress, identity, host,
  handoff, configuration, supervisor and evolution cases as applicable.
- On the final source, minimum **23/23** passed in 2.526 seconds (2,833 outside
  the filter) and all features **31/31** passed in 3.490 seconds (3,389 outside).
  The command selects `--lib --test feature_ingress --test daemon`, filtering
  `test(session_identity::) | binary(feature_ingress) | binary(daemon)`; the
  full profile additionally selects `test(harness::acp::runtime::)`. Thus both
  mixed extraction tests run in minimum, and all four relocated lease-runtime
  tests run in full. No process shared the user's existing daemon or storage.
- Final strict Rust 1.97.0 Clippy passed for workspace/all-features/all-targets,
  empty-feature application/all-targets and core-only application/all-targets.
  Full-workspace and empty-feature strict Rustdoc passed.
- Formatting and patch-whitespace checks passed. Source/manifests were frozen
  during runtime sequences. Logs use `/tmp/bitrouter-p0-session-runtime-*`.
  Existing stable linker and third-party future-compatibility warnings remain.

All final A01–A16 gates remain open. The supervisor implementation still needs
separation from shared local-command data/clients before its entire module can
be harness-gated. Remaining harness/TUI dependencies, server-tool/database flags,
capability-aware artifact identity, harness SDK delivery, historical/backend/
packaged-process/performance evidence, whole-candidate validation and final
independent review still precede the requested PR. P1 remains excluded.

## P0 5 database driver features (2026-10-16)

This stage follows `d83989c`. SQLite remains unconditional; PostgreSQL and MySQL
now activate through application `db-postgres` and `db-mysql` features, retained
in the full default product. Shared ORM/migration declarations no longer force
both drivers into every application profile. The MySQL-specific trajectory
error downcast follows its driver feature; generic SQLSTATE classification and
SQLite behavior remain unchanged.

A parsed PostgreSQL (including `postgresql`) or MySQL URL requires its compiled
driver before connecting. The same guard runs through host assembly, reload and
explicit upgrade preflight. Saved configuration still parses, while unavailable
activation reports the missing feature without including connection credentials.
Independent review caught URL-bearing CLI context layers that defeated that
specific guarantee. Those wrappers now use static contexts. The real-process
regression invokes `bro key sign` for each absent scheme in both JSON and human
modes, verifies the structured JSON message and checks both output streams for
the fixture password. Follow-up source review found no actionable issue. This
is a missing-driver diagnostic guarantee, not a claim that every existing
underlying-driver error has been audited for credential redaction.

Validation (non-incremental, debug symbols disabled, two build/test workers):

- Application/all-target minimum check passed. Isolated normal/build dependency
  trees for minimum, PostgreSQL-only, MySQL-only and all features show SQLite
  always present and each remote driver only in its selected profile.
- Before the CLI-context correction, the affected DB, trajectory-store and
  feature-ingress nextest selection passed **72/72** in each profile: minimum
  3.913s, PostgreSQL-only 3.814s, MySQL-only 3.895s and full 4.822s. The filter was
  `test(db::) | test(trajectory::store::tests::) | binary(feature_ingress)` with
  `--lib --test feature_ingress`. These exercise SQLite and feature availability,
  not live PostgreSQL/MySQL execution.
- On final source, `feature_ingress` passed **4/4** for minimum in 1.936s and
  **4/4** for all features in 2.985s. Missing-driver cases execute in minimum;
  the full profile instead retains the enabled gateway/listener process check.
- Rust 1.97.0 strict Clippy passed for full workspace/all-targets and application
  minimum/PostgreSQL-only/MySQL-only all-targets. Final full and minimum passes
  followed the CLI correction. Final strict Rustdoc passed for full workspace
  (38.75s) and minimum application (31.12s).
- Formatting and whitespace checks passed. Logs use
  `/tmp/bitrouter-p0-database-features-*`. Source/manifests were frozen during
  runtime sequences; existing linker/future-compatibility warnings remain.
  One initial nextest invocation supplied both `-j` and `--test-threads`, which
  nextest treats as duplicate test-thread options; it ran no tests and was
  corrected to `--build-jobs 2 --test-threads 2`.

CI now checks both absent driver closures and runs the database feature fixture
for individual remote-driver profiles. Live backend migration/read/write evidence
remains open under A09. All final A01–A16 gates remain open, including remaining
harness/TUI/MCP dependency isolation, server-tools gating, artifact capability
identity, harness SDK delivery and whole-candidate acceptance. P1 is excluded.

## P0 5 optional gateway server tools (2026-10-16)

This stage follows `c7b0c50`. Application `server-tools` enables MCP and remains
in the default distribution; it does not enable core or harness. The pipeline
module, builder field/method, upstream adapters, loop execution branches and
application construction of nested runners, Fusion aliases and BYOK backends
now follow that feature. Ordinary generation, Decisions, controlled native
calls, streams and settlement retain their original execution bodies. Native
bypass entry points remain available without the server-tool runtime.

Deployment configuration moved to `config::server_tools`: the root settings,
Fusion defaults and web search/fetch settings. Runtime `ServerToolLoopConfig`
and request-declaration parsing remain with the gated implementation. The moved
configuration fields/defaults/tests are unchanged except owning paths; both web
configuration files moved byte-for-byte. No AI or SDK runtime was introduced.
Six explicit tool selections fail before service activation without the feature,
consistently through assembly, reload and profile preflight. Loop tuning alone
is dormant; the existing real gateway process fixture now serves inference with
`server_tools.max_iterations` present and no selected tool.

Test gates retain generic coverage: eight mixed pipeline tests keep their
assertions and gate only tool-loop setup. Four true pipeline-loop tests, six
continuation-loop tests and one HTTP loop test follow the feature, along with
exclusive helpers. Shared continuation state/request/terminal-validation helpers
remain available. Assembly loop tests and runtime module unit tests follow their
implementation; configuration parsing tests remain unconditional.

Validation uses stable runtime builds and Rust 1.97.0 strict Clippy/Rustdoc,
non-incremental builds with debug symbols disabled and two build/test workers:

- Initial minimum all-target check passed with an unused server-tool timing
  field warning; the field and its ordinary initializer now follow the feature.
  Initial minimum runtime selection passed **580/580** in 7.873s. Strict Clippy
  caught that same warning before the correction; no lint suppression was added.
- Final affected nextest selection passed minimum **580/580** in 8.877s,
  server-tools-only **708/708** in 9.147s and all features **707/707** in 9.786s.
  Targets were `--lib --test feature_ingress`; the expression selected
  `gateway::pipeline::`, `gateway::http::`, `continuation::`, `config::`,
  `assemble::server_tools_tests::` and the process fixture binary. Filtered-out
  counts were 2,133, 2,224 and 2,699 respectively. Profiles contain different
  feature-specific tests, so their counts are not interchangeable coverage.
- Final strict Clippy passed for minimum application/all-targets (43.95s) and
  full workspace/all-targets (1m12s). Strict Rustdoc passed full workspace
  (43.43s) and minimum application (30.09s).
- Independently generated full/minimum configuration schemas are byte-identical
  to each other and the committed schema artifact. Formatting and patch checks
  passed. Logs use `/tmp/bitrouter-p0-server-tools-*`.
- Independent review found no additional issue beyond the timing-field gate;
  follow-up inspection confirmed that correction and no remaining finding.
  Source/manifests stayed frozen through runtime sequences. Existing linker and
  third-party future-compatibility warnings remain.

CI exercises optional server tools and ingress availability under both MCP-only
and server-tools profiles. All final A01–A16 remain open: remaining harness/TUI/
MCP dependency isolation, artifact capability identity, harness SDK delivery,
historical/live-backend/packaged-process/performance evidence and whole-candidate
validation/review still precede the requested PR. P1 remains excluded.

## P0 5 live gateway database backends (2026-10-16)

This stage follows `70074c8`. Live backend execution exposed two baseline MySQL
defects, distinct from the earlier driver-availability checks. Migration 12's
owner-scoped pending-outbox index exceeded the default utf8mb4 InnoDB key limit;
the failed migration left its tables and first four indexes committed, with only
the first eleven migrations in the ledger. Its MySQL definition now uses
64-character timestamp prefixes in that nonunique index. Stored values, full
owner/outbox keys, uniqueness constraints and SQLite/PostgreSQL index definitions
are unchanged. MySQL checks existing indexes explicitly so the same migration
can finish after partial DDL without deleting tables or rows. No migration ID,
order or ledger format changes.

After migration recovery, the backend test exposed the user upsert generating
invalid MySQL `ON DUPLICATE KEY IGNORE` syntax. It now uses SeaQuery's documented
primary-key self-assignment polyfill. PostgreSQL/SQLite retain `DO NOTHING`,
existing users retain their creation timestamp, and reserved-user rejection is
unchanged.

`database_backends` contains a normal temporary-file SQLite case and an explicitly
ignored PostgreSQL/MySQL case requiring a dedicated test URL. UUID-scoped fixture
records support repeated runs without dropping/truncating databases. The common
scenario checks the full migration ledger, repeated migration, users/keys,
metering upsert and scoped spend, trajectory start/route/settlement idempotence,
cross-owner rejection, pending outbox content, direct migration-12 rerun over
populated tables, and persistence after pool teardown/reconnection. These are
store-level backend checks, not historical-payload or independent-process
gateway acceptance.

Local execution used isolated PostgreSQL 17 and MySQL 8.4 containers, with default
MySQL utf8mb4 settings. The original partially applied MySQL database recovered
all 26 ledger entries and retained a pre-migration sentinel user unchanged.
Fresh databases exercised bootstrap separately from recovery. CI provisions one
server per backend job with its corresponding driver feature and runs the same
scenario; the ordinary SQLite case also joins the existing Ubuntu integration
shard. Development documentation records the explicit test URL and commands.

Validation uses stable runtime builds and Rust 1.97.0 strict Clippy/Rustdoc,
non-incremental builds, debug symbols disabled and two build/test workers:

- Existing all-feature migration unit selection: **14/14 passed**, 0.424s.
- Final MySQL-only integration on the recovered database: **2/2 passed**, 0.23s.
  A fresh utf8mb4 MySQL database: **2/2 passed**, 0.80s. Each run also exercises a
  separate temporary SQLite file.
- PostgreSQL-only integration on a fresh PostgreSQL database: **2/2 passed**,
  0.39s, including its temporary SQLite case.
- No-feature SQLite profile: **1 passed, 1 explicitly ignored**, 0.05s; the
  ignored server case requires its URL and driver rather than silently falling
  back to SQLite.
- Final strict Clippy passed for full workspace/all-targets (1m03s) and minimum
  application/all-targets (36.80s). Strict Rustdoc passed for full workspace
  (34.61s) and minimum application (27.30s).
- Final all-feature related unit selection passed **244/244**, 7.268s. The filter
  was `test(auth::) | test(metering::) | test(trajectory::store::) | test(db::)`
  with `--lib`; 3,158 unrelated cases were filtered out.
- Formatting, whitespace and workflow YAML parsing passed. Independent review
  found no blocking issue and one overly broad test-header claim about partial
  migrations; it now explicitly covers only the observed migration-12 state.
  Logs use `/tmp/bitrouter-p0-backend-*`. Existing linker/future-compatibility
  warnings remain. This records local execution; the new CI jobs have not run
  remotely yet.

The source audit also identified other existing `OnConflict::do_nothing` uses in
shared evolution-record initialization, ACP capture and core reliability writes.
Those paths need their own real-backend evidence; inserted-versus-duplicate
outcomes must not be changed through a blanket polyfill replacement. They remain
follow-up work, alongside historical data/profile transitions and the rest of
A01–A16. This stage does not establish multi-instance semantics or complete P0.
