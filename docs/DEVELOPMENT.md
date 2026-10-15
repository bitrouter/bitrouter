# Development Guide

This document is the workspace-level guide for BitRouter internals. Start with [`README.md`](../README.md) for the product introduction, then use this guide when you need to understand how the workspace is assembled or how to extend or operate the service.

## Workspace Architecture

The table below describes the current implementation during P0. Core, harness,
TUI, telemetry, model/provider runtime and the old SDK server have moved into
the application. The SDK provides gateway HTTP/SSE, control, evaluation and
managed core transport clients; application feature isolation remains in progress. The final
architecture and migration gates are in the
[service architecture refactor plan](BITROUTER_SERVICE_REFACTOR_PLAN.md).

BitRouter is a Cargo workspace organized into `crates/` for shared libraries and contracts, `apps/` for product hosts, and `extensions/` for independently delivered capabilities. Each extension groups its related Cargo packages in one directory; `helpers/` contains contributor tooling.

| Crate                            | Tier    | Responsibility                                                                                                          |
| -------------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------- |
| `crates/bitrouter-ai`            | crate   | Lightweight model/protocol, lifecycle, extension, observation, catalog and managed-attempt evidence contracts; optional schema derives; no service runtime or transport |
| `crates/bitrouter-sdk` | crate | Gateway HTTP/SSE, control and evaluation clients using AI wire contracts; optional managed core WebSocket transport with caller-owned checkpoint ACKs |
| `extensions/regex-checker/matcher` | extension library | Rules and native request-check callback using AI contracts; legacy hooks live in the application |
| `apps/bitrouter`                 | app     | Assembly library + the `bro` CLI binary (package/lib stay `bitrouter`) — turns a `Config` into a running `App`, owns core scheduling, harness execution/storage, terminal UI, optional OTLP and management commands |
| `helpers/dist-helper` | helper | Unpublished registry/chart tooling; configuration schema delegates to the application-owned generator |

The `extensions/` directory expresses ownership and delivery boundaries; it does not create a loader or force a transport. Cargo packages remain ordinary Rust libraries or binaries. Custom hosts register native request checks through `bitrouter::gateway::extensions::registry::ExtensionApi` inside `assemble::build_app_with_extensions`; the application also retains legacy `Plugin`/hook interfaces for trusted host assembly. See [the extension directory guide](../extensions/README.md) and [guardrails](../extensions/regex-checker/README.md).

For the native runtime module map, execution reading order and test layout, see
the [core and harness source guide](../apps/bitrouter/HARNESS.md).

### External interfaces

Clients reach BitRouter through its external **interfaces** — the ways *in*. These are distinct from the gateway's internal *wire-protocol adapters* (Chat Completions / Responses / Messages, described below): an interface is an entry point, an adapter is a dialect the `language_model` pipeline parses and speaks.

| Interface                 | Where it lives                                                                                            | Entry point              |
| ------------------------- | -------------------------------------------------------------------------------------------------------- | ------------------------ |
| **API** (HTTP LLM router) | application `gateway::http` over `gateway::pipeline` | `bro serve`        |
| **MCP gateway**           | application `mcp` feature and gateway assembly: downstream `/mcp` aggregation, configured upstream clients, Skills-over-MCP relay, and server-side tool execution | `POST /mcp`; `bro mcp check` |
| **ACP**                   | application `harness` feature (`apps/bitrouter/src/harness/acp/`): `controller` is the ACP-client-facing server, `client` the transport-generic ACP client, `up` the agent-process transport, and `translate` the typed view of `session/update` used by `run`. The client-facing stdio bridge, one-shot runner, and Code session all consume the same controller/client stack. Subcommand glue lives in `apps/bitrouter/src/acp_cli.rs`. | `bro acp serve <agent>`; `bro run` |
| **Native ACP** | `bitrouter::harness::acp::native`: negotiated v1/v2 handlers, projectors, approval delivery and native controls over the host's single ThreadService. `apps/bitrouter/src/native_acp.rs` owns the OS-local endpoint and stdio bridge; EOF detaches. | bare `bro acp serve` |
| **ACP (interactive)**     | `apps/bitrouter/src/tui/code.rs` owns conversation state and rendering; `apps/bitrouter/src/chat/code.rs` drives asynchronous effects and ACP turns, with injected services in `actions/code.rs` and the shared `SessionHost` in `acp_cli.rs`. | `bro code <agent>` |
| **BRO native agent** | `bitrouter::harness::service::ThreadService` owns one Thread/Turn execution path, durable context, FIFO/steering, owner/workspace fencing and bounded observation. App owns the database and transport adapters. Safe hot unload/reload and cold public queries share the same authority. | Bare `bro code` (`--thread-id`); `bro task run` (create/start client); local v17 native socket (including cold Thread directory); opt-in `/agent/v2` HTTP API |
| **CLI**                   | `apps/bitrouter` — the composition-root binary                                                            | `bro <subcommand>` |

The TUI is an internal application module. It consumes shared presentation
values and command vocabulary; drawing code does not access provider credentials,
database repositories or service execution. `source_boundaries` checks these
imports now that Cargo no longer supplies a separate crate boundary. Permission
policy belongs to the harness, and command resolution belongs to `commands`.
Cost attribution remains explicit, missing cost is unreported, and cancelling a
permission prompt never grants consent.

Terminal-free log tails and shared values live in `presentation`. Plain ACP
transcript formatting still uses the old renderer while its neutral formatting
is extracted. Complete headless dependency isolation remains a P0 gate.

BRO native tools are `read`, `glob`, `grep`, `write`, `edit`, and `shell`.
`read` dispatches known workspace paths to file lines or direct directory
entries. The three inspection tools form the read-only profile. The server
resolves one interpreter before a coding execution; model commands and
verification share it. See [the six-tool contract](BRO_BASE_TOOLS_SPEC.md).

The host constructs `ThreadService` and binds task/native ACP endpoints only
with `harness`. The supervisor and its recovery ledger, and the in-memory ACP
route-lease runtime, also require harness. A reduced gateway leaves those
endpoint paths, supervisor records and workspace resources untouched. Request
identity normalization, accounting and existing session-spend reads remain
available without that runtime. Configured MCP upstreams require `mcp`; enabled agent HTTP
ingress requires `harness`. Startup, reload and profile preflight reject missing
capabilities before activating the service. This runtime separation does not yet
complete the remaining harness/TUI dependency isolation.

SQLite is available in every application build. PostgreSQL and MySQL drivers
require `db-postgres` and `db-mysql`, both enabled by the default distribution.
Reduced builds preserve configuration parsing and reject an unavailable selected
backend before connecting, including reload and profile preflight. Driver
selection does not provide multi-instance coordination.

Server-tool deployment configuration lives under `config::server_tools` and is
always readable. The tool loop, nested inference runner, Fusion alias and search/
fetch backends require `server-tools`, which enables `mcp` and is part of the
default product. Selecting any server tool without that feature fails before
service activation, consistently on startup, reload and profile preflight.
Loop tuning alone is dormant. This request-local loop is independent of the
durable harness and core scheduler.

The native BRO runtime has one `ThreadService` per harness-enabled serving instance. App-owned
SQLite stores Thread/Turn facts, acceptance keys, public events and owner/version
fences. Clients only project state; detaching does not cancel, approve or resume.
Bounded tool workers, ordered exclusive effects, FIFO/steering, joined shutdown,
workspace locks/markers and bounded startup discovery preserve execution barriers.
Cold queries do not execute. Safe same-owner reload and explicit stopped-owner
checkpoint recovery preserve identities and budgets without replaying completed
effects; lost owners, unknown effects and uncertain accounting remain blocked.

Managed attempt evidence used by the core's existing `model.evidence` command
now lives in `bitrouter_ai::core::evidence`. Import `NativeAttemptReport`, its
route/usage evidence and private-context/continuation observations there.
These data structures confer no ownership or execution authority. The gateway
still validates provider provenance, projects raw usage, computes commitments
and enforces report limits; the core still decides whether evidence may be
admitted.

Existing managed wire commands, responses, limits and HTTP error envelopes live
in `bitrouter_ai::core::protocol`; durable checkpoint envelopes and ACK data live
in `bitrouter_ai::core::checkpoint`. The application retains exact-byte encoding,
digest verification, append admission and the commit barrier. Trusted host
consumers import `CheckpointBatchExt` from `bitrouter::storage::checkpoint` for
the existing codec/admission methods, and `HarnessManifestExt` and
`ToolResultLimitsExt` from `bitrouter::storage::checkpoint::validation` for
shared manifest and frozen tool-result validation. `ClientMessageExt` remains
in `bitrouter::core::protocol` for live connection-grant checks; `CommitGate`
remains in `bitrouter::core::checkpoint`. These traits add methods to the shared
data; they do not provide a plugin interface or transfer persistence authority.
The shared codecs do not depend on core execution. Private persisted session
records now live in `bitrouter::storage::checkpoint::snapshot`, including
`SessionSnapshot`, its routing/collaboration/accounting records and context
evidence. Offline immutable-history validation and bounded observation projection
live below `snapshot::context`. These remain application storage types;
AI/SDK checkpoint envelopes carry the state opaquely. Public observation DTOs
must not expose the complete private snapshot. Core retains live context
selection, collaboration, accounting transitions and session execution.
The scheduler and its local native drivers are gated by `core`. Without core,
native session inspection and observation validate stored records, but execution
mutations fail before workspace discovery or execution-owner acquisition. HTTP,
local IPC and native ACP report this distinction. The host skips native execution
initialization in that profile, and offline shutdown leaves queued work intact.
Complete harness-only dependency isolation and the minimum feature matrix remain
in progress; the core-disabled checks currently retain the other default runtime
dependencies.

The SDK provides the existing core capability HTTP method and, with its client
`core` feature, the managed WebSocket channel. Clients explicitly persist and
acknowledge checkpoints; the SDK neither executes tools nor replays commands
after uncertain delivery. This preserves the existing service transports rather
than establishing complete core HTTP coverage.

Trusted application hosts that previously called
`NativeEvidenceCommitment::capture` use
`bitrouter::gateway::pipeline::native_report::commitment`.
`NativeCacheObservation::capture` becomes
`bitrouter::gateway::pipeline::native_accounting::capture_cache`.
Those algorithms are application APIs, not AI helpers or SDK transport code.

Use [the standalone runtime contract](BRO_AGENT_RUNTIME_SPEC.md) for current
requirements, including root format 2 and local v17/HTTP v2. The
[runtime acceptance index](BRO_AGENT_RUNTIME_IMPLEMENTATION.md) separates local
fixtures, historical provider/platform checks and remaining product gates.
[Conversation UI](BRO_CONVERSATION_UI_SPEC.md) and
[six tools](BRO_BASE_TOOLS_SPEC.md) specialize that contract. Older phase ledgers
are historical references, not competing descriptions of current behavior.
Future [core/harness separation](BRO_AGENT_RUNTIME_HANDOFF.md) assigns live
scheduling/checkpoint semantics to Core and durable storage/ACK authority to
Harness; the standalone implementation does not prove that integration.
Router metering and external ACP session ownership retain their own boundaries.

The CLI is the **host** interface: it owns `main()` and mounts the other
interfaces as subcommands. The gateway owns model/MCP interop; the harness
owns ACP execution. The application also owns local actions and presentation.

### Dependency Logic

**`bitrouter-ai`** owns public model/protocol data, lifecycle and capability
contracts, observation vocabulary and catalog artifact types. Its default
production graph contains serde, serde_json and thiserror; `schema` adds
schemars. Neither default nor all-public-features builds include transport,
provider authentication, an async runtime, a database, terminal UI or OTel.

Application **`gateway::model`** owns protocol codecs, conversion admission,
selected-model HTTP invocation, timeouts and stream processing.
**`gateway::providers`** owns selected-account authentication and provider
adaptation. Their inputs are explicit; invocation does not discover accounts,
start login or select routing fallback. Mutable pipeline contexts, finalization
and retry policy remain application gateway behavior.

Native Decisions uses `DecisionRequest`/`DecisionResult` and
`ModelClient::decide`. Gateway envelopes distinguish `PipelineInput::Generation`
from `PipelineInput::Decisions`, and the output variants likewise retain their
semantic operation. The HTTP gateway serves `POST /v1/decisions` through the
shared execution, delivery and settlement lifecycle. Streaming and generative
continuation/tool defaults are rejected or scoped before native dispatch.

Routing selects a compatible operation before preferred-protocol matching.
`ModelTarget.api_protocol` determines the gateway model client's outbound wire. Existing
generation calls prefer a supported inbound wire or the first configured
generation wire; the enum/unknown-host fallback is Chat Completions.

Route hooks may capture evidence in `after_resolve`, after every mutable
`resolve` hook and operation/effort filtering. App-owned `CaptureTariffs` freezes
each effective target's independent protocol tariff and endpoint profile here.
Published tariffs retain their optional `endpoint_profile` provenance across
registry merge and configuration changes, so global rates cannot silently rebind
to a regional endpoint. Legacy/custom tariffs without a declaration bind to the
configured endpoint. Metering and evaluation use that snapshot; `UsagePricingSnapshot` projects the
same rates to the gateway's conservative stream usage selection. Unknown frozen
prices disable live price lookup. Custom hosts composing `MeteringRecorder`
must also install `recorder.tariff_capture(require_known)` at the route stage.

`server.require_known_pricing` is opt-in and restart-required. It rejects routes
without guaranteed price coverage before I/O, including native Decisions while
cache billing remains unverified. Prices and endpoint profile anchors also
require restart. Native nonzero cache usage retains counters and raw evidence
with an unavailable cost. See [Decisions specification](DECISIONS_API_SPEC.md)
and the shipped [setup reference](../skills/bitrouter/references/decisions.md).

Codex subscription `generate` calls use upstream SSE and the gateway's canonical stream
collector; the HTTP executor shares that collector and retains its gateway request policy
and execution envelope. Collection requires a terminal and consumes through EOF
before returning success. It preserves fields already present in canonical
events. Completed Responses reasoning now has a redacted AI-owned native capsule
carrying the final item payload. Responses JSON/SSE output retains its identity,
opaque encrypted content and native summary/content fields, while visible text
lanes remain separate. Snapshot consistency checks prevent native data from
restoring text changed by hooks. Responses ingress retains native reasoning, including
encrypted-only items. Native replay still requires future target/account authority
proofs. AI's content-free `ConversionReport` covers initial native-reasoning and
structured-output admission; direct invocation and gateway HTTP preflight use the same
request preparation. Router exclusions reach `ObserveHook::on_conversion_excluded`
before hop observers or provider execution, while an all-excluded chain returns a
typed incompatibility report. Other client codecs return output incompatibility
when they cannot retain native reasoning. Gateway status mapping stays in the gateway.
Built-in request parsing now also refuses initial ingress omissions before any partial
Prompt can enter the pipeline: unknown items/blocks, unsupported content values,
lossy system/tool-result shapes and unsigned Messages thinking.
Reports carry original wire indices and remain content-free. HTTP handler tests verify
400 responses before any executor call in ordinary and streaming modes. Shared request admission now also excludes initial structural history losses: omitted
reasoning/sources, provider execution or MCP identity lost in projection, unsupported
approval parts/reasons and omitted tool-result media/file IDs. Existing native-family
tool declarations remain eligible; unknown cross-family translation and non-object
argument loss are refused. Native continuity tokens cannot cross a wire that omits them. Native token
presence/representation and local denial pairing are not complete authority proofs.
Projection now also refuses explicit strict flags, schema names/descriptions and file
names that the selected codec would omit, plus tool-result error/denial status loss.
Google Chat admits a conservative target-scoped schema/option subset without rewriting constraints. Explicit cache resources and unverified strict/schema metadata are refused. A suppressed paired denial cannot lose its supplied reason.
Malformed argument strings cannot become empty objects on Messages. Chat and
Messages preserve text-only tool-result arrays, and Chat preserves multiple text parts.
Unclassified Chat ordering/reasoning concatenation and selected three-wire ingress
attribute/boundary losses are refused. Value-preserving JSON encoding into
string-only tool slots remains eligible; error/denial status is checked separately.
`ConversionReport.admitted` retains classified JSON encoding separately from refusal issues.
Selected-target preparation can return
the report through `ModelClient::render_request_with_report`; executor preflight returns
that assessment. The gateway rechecks refusals and invokes `on_conversion_admitted` for
eligible detected effects before hop start, without counting a candidate as an HTTP
attempt. Empty assessments emit no conversion callback. Remaining ingress attributes,
media/options/boundary effects, schema/model validation, other admitted classifications,
Core/durable recording, output effects and degradation authorization remain open.

`bitrouter-ai::catalog` owns the published provider/model artifact types.
Application `gateway::catalog` owns explicit bounded refresh, source-keyed stores
and retirement filtering. A catalog starts empty with memory-only
storage; loading and network permission are separate explicit operations. Failed
parse, validation or persistence retains its prior snapshot. `gateway::catalog::cache`
selects the 24-hour file cache and startup/reload refresh policy; `catalog::bundled`
supplies the committed offline baseline for the default public source. Unbound
legacy caches are preserved but not adopted as an assumed source. Activation,
credential lookup and user-config merge remain in the application/provider bridge.
The AI DTO decoder performs no runtime filtering or logging. The gateway decoder
quarantines retired entries when loading bundled data, cached snapshots or fetched
artifacts; existing cache bytes remain unchanged. Published pricing-profile values
also stay in AI, while endpoint applicability is checked by gateway routing.

Application `gateway::providers` implements selected-account authentication
and provider adaptations. Generic OAuth decoding, refresh, device authorization,
PKCE, callback listeners and explicit-path credential stores live beneath
`gateway::providers::auth`. The old AI `hosted`, `pkce`, `hosted-login` and
`file-store` features are removed; these mechanisms belong to the gateway.
Product activation, environment capture, permitted CLI/Keychain adoption and
file-path policy remain application concerns. `LoginUx` supplies interaction and
callback HTML; the login implementation retains state/redirect validation and
the existing 15-minute product deadline. Dropping an async login releases its
listener; already-started blocking stdin work remains a lifecycle limitation.

`gateway::model::target::ModelTarget` holds effective connection values,
compatibility and the selected account label. Its credential values and
`AuthScheme` are application types. Public catalog compatibility data remains
in AI because distributed catalog artifacts contain it; that does not move
credential selection or compatibility enforcement into the contract library.

`OAuthSession` owns admitted refresh and persistence independently of a dropped
caller future. Ordinary credential files preserve existing labeled/legacy
formats and path precedence; reads do not rewrite data. Refresh and
administrative writes use the same file binding, including aliases and relative
paths bound at construction. The file backend preserves unrelated entries,
rejects commits after login/logout replaces the selected credential, and retains
failed rotated replacements in memory for a later commit. This remains
process-local coordination, without cross-process exclusion or crash recovery.
Explicit credentials outrank stored values; a permitted fallback applies only
when the selected slot is absent. Copilot caches remain source-bound.

`gateway::providers::hosted::HostedSession` retains full token envelopes and
origin binding. Application account assembly shares that session across model,
management and telemetry calls. Hosted device login, refresh and revocation use
the same metadata/token definitions; account settings, scopes, paths and storage
policy remain with application assembly.

Metered Google uses the official Chat endpoint; the retired native Gemini and
Antigravity transports are not restored. Tool signatures retain canonical
metadata and a selected-credential replay proof. Clients echo the entire
`extra_content`; the gateway removes its proof before upstream dispatch.
See [retirement progress](GEMINI_PROTOCOL_RETIREMENT_PROGRESS.md) for the existing
SDK limitation and live-provider validation scope.

The matcher depends only on AI capability contracts. The repurposed SDK is a
service-client package and never owns gateway assembly, configuration discovery
or provider authentication. Its gateway/control/evaluation methods use explicit endpoints and
credentials, bounded reads and no automatic replay. See the
[SDK guide](../crates/bitrouter-sdk/README.md) for the available methods and
[service progress](BITROUTER_SERVICE_REFACTOR_PROGRESS.md) for remaining P0 work.

The span names, attribute vocabulary and lifecycle invariants belong to the AI
observation contract. The committed schema and conformance tests preserve those
semantics independently of their optional OTLP rendering.

Application `telemetry` owns OTLP transport, bearer resolution, batching,
endpoint configuration, cardinality limiting, span construction and the tracing
bridge. It consumes AI's observation schema and implements the pipeline's
`ObserveHook`. Other observers continue to record essential request facts and
usage even when OTLP is absent. The contract carries no OTel types. See the
[service plan](BITROUTER_SERVICE_REFACTOR_PLAN.md) for the target boundary and the
[telemetry spec](TELEMETRY_CRATE_SPEC.md) for frozen wire behavior.

Because the schema is the contract, it is written down rather than inferred from a renderer's call sites. `crates/bitrouter-ai/src/observe/schema.rs` declares every span, attribute, event, metric and silent-failure invariant, names no `opentelemetry` type, depends on nothing but `serde`, and renders to the committed artifact `crates/bitrouter-ai/span-schema.json` — regenerate with `UPDATE_SPAN_SCHEMA=1 cargo test -p bitrouter-ai committed_artifact` (default features: the module is ungated, and a staleness guard that only fired under `--all-features` would let the artifact rot everywhere else), and the ordinary test run fails when it is stale. It ships with the AI contract crate because it is the interop surface. The declaration is `pub` for the same reason: a second renderer needs it at compile time, not just as JSON. The three helpers only a conformance suite calls — `span_def_for`, `value_type_matches` and `render_json` — sit behind the default-off `testing` feature instead, since public API with no production caller is what CLAUDE.md rule 4 forbids; the feature carries no dependency, so a renderer enables it under `[dev-dependencies]` and pays nothing. Conformance tests in application `telemetry/exporter.rs`, `telemetry/acp.rs` and `telemetry/http_layer.rs` drive real lifecycles and assert that nothing reaches the wire the declaration does not describe, so the artifact is checked rather than aspirational. See [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md).

The application gateway owns `assembly`, `http`, `pipeline`, `mcp`, `routing`,
`observability` and `extensions`. Configuration lives in application `config`.
The generation pipeline retains its hooks, request checks, execution, stream
processing and settlement; MCP has its own routing contract. ACP belongs to
harness execution, not a gateway protocol pipeline.

Serving-time workflow extraction, identity, predictor contracts and decision
records live in `gateway::routing::workflow`. Published experiment state and
stable assignment live in `gateway::routing::exploration`; certificate identity
and compiler-digest helpers live in `gateway::routing::experiment_identity`.
Learning and replay consumers import these owners rather than supplying a second
projection or assignment algorithm. The existing model/provider wire formats,
experiment digest domains and policy-decision records are unchanged.

Gateway `observability::decision` owns bounded pending policy observations and
routing receipts; `observability::prediction` owns the response observer.
`observability::settlement` captures the owner-bound decision once, before any
awaited trajectory lookup, and delegates ordinary request evaluation only when
that consumer is assembled. Cache expiry or eviction during the lookup cannot
silently replace or erase the captured decision. Shared durable evaluation
validation/digests live in application `storage::evaluation`, without publishing
service algorithms through AI.

Trajectory settlement always follows authoritative metering and persists its
terminal fact and outbox envelope together. Without core, assembly does not
construct an eval service/publisher or mount eval HTTP routes; outbox rows remain
pending and retention protects them. Core-enabled assembly retains startup,
request-triggered and shutdown drains. Evaluation, outbox publication, policy
compilation/publication, optimization, adequacy and workflow research now live
under `core::{eval,policy,optimization,adequacy,workflow_state}`. The file-based
policy producer is `core::policy::files`; gateway `policy_lock` retains published
artifact loading, validation and operator configuration/rollback operations.
`eval`, `optimize`, `workflow-state` and policy compilation/publication commands
require core, as does `BITROUTER_WORKFLOW_TRACE_JSONL` capture. Unavailable work
is rejected before its input files or execution stores are opened. Ordinary
trajectory inspection/replay/pruning remains available without core.

Checkpoint learning, scoring, operator controls and the background worker now
live in `core::evolution`. Published block contracts, allocation draws, session
membership, request intents and live dispatch fences belong to
`gateway::routing::blocks`. Every build uses the same serving implementation,
including adopted policies while evolution mode is Off. Core supplies posterior
learning and operator/publication mutations through separate implementations.
Without core, no evolution worker starts; unavailable CLI and local control
operations reject execution, and the TUI omits its evolution panel. Ordinary checkpoint storage operations retain
their existing harness owner. This does not establish minimum-gateway closure:
harness, TUI, MCP and the remaining feature dependencies still require isolation.
The progress ledger records the outstanding validation.

Reserved judge request admission and attempt bookkeeping now belong to
`gateway::metering::judge::JudgeCosts`. They retain their pre-authentication
ordering and existing dispatch/replay checks. Always-run accounting and
evaluation consume the replay markers from `gateway::metering::replay` instead
of depending on a learning runtime. Job reservation construction and cost-report
operations remain in `core::evolution::costs`; their implementation extends the
gateway-owned handle without changing its admission behavior.

The supporting record store and digest live in `storage::evolution`. Persisted
execution, settlement and route-intent data live in `storage::execution`, the
native `SessionIdentity` index in `storage::session_identity`, and shared
capture ORM definitions in `storage::capture`. Capture, gateway and learning
consumers use the same definitions. Storage table names, digest domains,
serialized fields and validation remain unchanged. These are internal application
records, not new SDK contracts or authority to execute a session.

`gateway::observability::requests::GatewayRequestTracker` owns canonical request
identification, duplicate rejection and admission inventory, plus hop, terminal
and settlement observation. The host gives its inventory handle directly to
capture-registration consumers so they share the serving epoch. The tracker runs
before learning selection. `CanonicalRequestBinding` then creates the bound
execution record and validates routing/policy generations and the recorded
session before dispatch or context revalidation. `EvolutionRuntime` supplies an
optional selection and retains live learning-control validation. Gateway
bookkeeping does not construct the learning service or mutate canonical
conversation content. Optional learning
startup, source and command gates remain unfinished; this move alone does not
establish the complete minimum-gateway dependency closure.

`bitrouter-ai` contains no model execution or authentication implementation.
Optional guardrails adapters and telemetry remain application modules; enabling
a Cargo feature alone does not register a checker or opt into export.

### Extension authors and host assembly

New request-check authors use an ordinary function accepting
`bitrouter::gateway::extensions::registry::ExtensionApi` and call
`request_check(id, revision, callback)`. Business inputs and decisions live in
`bitrouter_ai::extension::request_check`, without a wire envelope. The custom host passes the
registration function to `host::serve_with_extensions` for the shared foreground
daemon lifecycle, or `assemble::build_app_with_extensions` for low-level embedding.
Fragment and coverage types also live in `bitrouter_ai::extension::request_check`.
Router bindings
control execution scope and order; registration is not global activation.
Beta supports compiled extensions only: no HTTP checker service, wire crate,
probe command, runtime installation or extra extension API crate.

Valid unconfigured registrations are inactive with sorted startup diagnostics;
configured instances require a matching registration even without bindings.
The regex example uses the same inference, local/remote management, reload and
shutdown path as `bro serve`. Restart custom hosts with their own executable;
the example does not implement the default CLI's background-launch protocol.

The restricted registrar does not expose host builders, migrations, credentials or mutable
pipeline context. The host retains resource bounds, fixed binding identity and
bounded content-free tracing. Synchronous callbacks cannot be forcibly
terminated by deadlines; they are trusted in-process code.

Legacy `Plugin` / `AppBuilder::plugin` and optional `GuardrailsPlugin` retain
custom-host global, stream/output and migration semantics. Input-only checks do
not replace those capabilities. The registrar and legacy host assembly now live in the application. `PluginId`, existing
`Config::plugins` consumers, Context extensions and external agent-plugin
manifests retain their separate meanings. See [migration](GUARDRAILS_EXTENSION.md).

### Service and client feature ownership

The old SDK `server`, `config_file` and `mcp` flags are retired. Gateway HTTP
and configuration parsing always belong to the application; concrete MCP
transports use application `mcp`. ACP uses application `harness`. Existing
`agents:` configuration and its schema remain compatible. The SDK currently has
no server API; its gateway/control/evaluation methods do not enable service features.

AI observation declarations are ungated. Its pure conformance helpers use the
default-off `testing` feature. The [P0 plan](BITROUTER_SERVICE_REFACTOR_PLAN.md)
defines the final feature graph and isolated validation matrix.

### Application telemetry feature flags

| Feature | Purpose |
| --- | --- |
| `otel` | Alias for `otel-http` |
| `otel-http` | OTLP/HTTP protobuf transport with refresh-aware bearer resolution |
| `otel-grpc` | OTLP/gRPC transport with configured static metadata |

Common exporter code is compiled whenever either transport is selected. Ingress
spans use the application's existing HTTP server; there is no separate `server`
feature or transport-less exporter flag. ACP spans additionally require harness.
Default installations retain HTTP export capability; endpoints remain opt-in.
Without either transport, export configuration still parses, basic tracing logs
and request accounting remain active, and harness-local ACP records remain
available. Explicit export opt-in fails before installation-ID or export-bearer
resolution at startup, reload and handoff preflight; dormant configuration stays
readable. Standalone ACP preparation applies the same availability check.
Keep `plugins.bitrouter-telemetry`, `BITROUTER_TELEMETRY_*`, `io.bitrouter.observe`,
the `bitrouter` meter and `bitrouter::observe::*` log targets unchanged.

## Request Flow

A streaming LLM request moves through the workspace like this:

1. The `bitrouter` binary resolves the config source (see *Configuration*), loads or synthesises a `Config`, and `assemble.rs` builds an `App` — the `language_model` pipeline with the builtin hooks wired on.
2. The application's gateway HTTP server receives the inbound HTTP request on one of the protocol routes and the matching **inbound adapter** parses it into a canonical `PipelineRequest` (model name, messages, tools, params).
3. The `language_model` pipeline runs its stages:
   - **Pre-request** — local policy hooks, followed by explicitly bound external request checks. Authentication and normalization occur before router admission.
   - **Route** — the `RoutingTable` resolves the model name to a fallback chain of `RoutingTarget`s (provider + upstream model id + protocol); `RouteHook`s may rewrite the chain.
   - **Execute** — the executor dials the first target; on failure the `FallbackPolicy` decides whether to try the next. The **outbound adapter** for the target's protocol renders the provider request and decodes the provider response (and its SSE stream).
   - **Settlement** — every `SettlementRecorder` runs (metering, etc.), success or failure.
4. For streaming, the canonical `StreamPart` stream flows through the `StreamHook` stage and is re-encoded by the inbound adapter into the **client's** protocol — so a client written for the Responses protocol can transparently use a Messages upstream, and vice versa.
5. `ObserveHook`s receive read-only lifecycle events throughout; application telemetry turns them into OTLP traces and metrics, and the binary's own observers consume the same events.

The gateway MCP pipeline has no model settlement; ACP execution has separate
harness session and lifecycle ownership.

## Configuration Model

### Config source resolution

When a subcommand doesn't pass `-c <path>`, the binary walks a fixed order (see `apps/bitrouter/src/paths.rs`):

1. **`-c <path>`** — explicit; a missing file is a hard error.
2. **`./bitrouter.yaml`** in the current directory.
3. **`$BITROUTER_HOME/bitrouter.yaml`** — if the env var is set, that file must exist.
4. **`~/.bitrouter/bitrouter.yaml`** — used if present.
5. **Zero-config in-memory defaults** — when nothing above exists. No file is written; `bro init` is the explicit way to scaffold one.

The daemon `chdir`s into the bitrouter home (the config file's directory, or `~/.bitrouter` for zero-config) on startup, so relative paths in the config — `database.url`, `server.control_socket` — and the socket / pid / log all resolve against one stable location.

### Zero-config and the provider catalog

In zero-config mode `bitrouter::providers::apply::zero_config()` builds a `Config` with `skip_auth: true`, `listen: 127.0.0.1:4356`, and the compiled-in hosted gateway auto-enabled when its API key is set in the environment. Every other public provider comes from the fetched-or-cached registry merge: an env-keyed registry provider becomes active when its credential is available, and a local-OAuth provider becomes active after `bro providers login <provider>`.

## HTTP Server Surface

The axum server is application-owned (`apps/bitrouter/src/gateway/http.rs`):

| Route                               | Handler                          |
| ----------------------------------- | -------------------------------- |
| `POST /v1/chat/completions`         | Chat Completions inbound         |
| `POST /v1/responses`                | Responses inbound                |
| `POST /v1/messages`                 | Messages inbound                 |
| `POST /v1/decisions`               | Native Decisions inbound         |
| `GET  /v1/models`                   | model catalog listing            |
| `POST /mcp/{server}`                | MCP gateway (JSON-RPC proxy)     |
| `GET  /metrics`                     | OTLP-migration banner (see below)|
| `GET  /health`                      | health check                    |

`GET /metrics` is retained for endpoint compatibility only. Prometheus accumulation was removed: metrics are now *pushed* over OTLP by application telemetry, and the endpoint serves a short banner pointing at `plugins.bitrouter-telemetry.otel` (`EmptyMetricsRenderer` in `apps/bitrouter/src/assemble.rs`). The gateway `observability::metrics::MetricsRenderer` trait and its `text/plain; version=0.0.4` content-type default still exist — a deployment that wants a real pull-based endpoint implements the trait itself.

Daemon mutation (`stop` / `restart` / `reload`) runs over a Unix domain socket.
When the optional authenticated HTTP control listener is enabled, it exposes
typed operator reads and reload operations behind its own bearer gate and
loopback exposure policy. It does not mount an MCP origin endpoint. See
`apps/bitrouter/src/{daemon,remote_control}.rs`. Portable control transport,
capability discovery and reload recovery now live in `bitrouter_sdk::control`;
`apps/bitrouter/src/client/control.rs` retains profile/environment integration
and CLI error classification.

## CLI Surface

`bro <subcommand>` exposes lifecycle and reports plus agent-facing entries: `task run` (BRO headless), bare `code` (BRO interactive), `launch <agent>` (native interface), `code <agent>` (ACP UI), `run <agent>` (headless turn), and `acp serve` (native v1/v2 ingress) and `acp serve <agent>` (external manager endpoint). `claude`, `claude-code`, and `codex` are native shortcuts. `requests` is the canonical request-history report; `mcp check [server]` inspects configured upstream MCP servers. Old `spawn`, `chat`, `tui`, `tools`, and related compatibility leaves are hidden. See `apps/bitrouter/src/main.rs`.

### Observability surface (`bro requests`)

One surface, and no module of its own. `RequestsReport`
(`apps/bitrouter/src/output/reports/requests.rs`) is an ordinary `CliReport`:
the builder in `main.rs` polls the metering store and the control socket, and
`Human::table` / `Human::status_block` render it. There is no second table
implementation and no terminal-only path.

The earlier status-only TUI module was deleted before the conversation renderer
was moved into `apps/bitrouter/src/tui/`. It had become a
618-line private implementation of one public function that bypassed `Output`
(so `--json` was silently ignored) and reimplemented `output/human.rs`'s table.
Its signal arm moved to `chat/signals.rs`, next to its only caller.

The current application TUI owns drawn ACP/native conversation UI; ordinary
request reports remain independent of that renderer.

The hosted mode `bro launch --tui` and its VT emulator (`tui/host.rs`, `tui/term.rs`, `tui/pty.rs`, `tui/conformance.rs`, `tui/fixtures/`) are **deleted**, along with the fidelity matrix that gated them and the `alacritty_terminal` / `portable-pty` / `termwiz` / `wezterm-input-types` dependencies. See [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) for the reasoning and for what replaces it — an inline-viewport ACP client rather than a terminal emulator.

`spawn::prepare` still builds the child once and `exec_inherited` runs it; the `Prepared` seam is kept independent of hosting.

## Where To Extend The System

### Add or update a provider

Add a provider definition under `registry/providers/*.yaml` (the registry source; `dist/` is regenerated by `helpers/dist-helper`). `bearer` / `header` auth needs no Rust. For a regional or per-account base URL, use `${VAR}` in `api_base` — it is resolved from the environment at merge time (e.g. Bedrock `https://bedrock-mantle.${AWS_REGION}.api.aws/v1`). For stateful auth (OAuth, token-exchange), add an injected `AuthApplier` impl in `apps/bitrouter/src/gateway/providers/` keyed by the registry `auth.handler` and register it in `apps/bitrouter/src/assemble.rs::build_auth_appliers` (see `copilot`). See [`CONTRIBUTING.md`](../CONTRIBUTING.md) for the step-by-step.

### Add a new wire protocol

Protocol adapters live in `apps/bitrouter/src/gateway/model/protocol/`. A new protocol needs an inbound adapter (parse request / encode response + SSE), an outbound adapter (render request / decode response + SSE), a variant on `ApiProtocol`, dispatch wiring, and coverage in the protocol-conversion test matrix.

### Add a provider whose wire isn't HTTP+JSON+SSE

Rare — no current registry provider needs this. The big clouds (Bedrock, Azure) speak one of BitRouter's built-in protocols over SSE and are ordinary Bearer registry providers. Only if an upstream uses a wire an existing outbound adapter can't decode (e.g. a vendor SDK's binary event-stream) do you add an `ApiProtocol::Custom` outbound adapter + `Transport` in a standalone crate, registered on the dispatch executor at startup. See the `Custom` escape-hatch docs in `apps/bitrouter/src/gateway/model/protocol/mod.rs`.

### Add a hook or compiled capability

Trusted custom hosts use application `gateway::pipeline::hooks` and
`gateway::pipeline::settlement` when they need global pipeline integration.
New request-check implementations consume the AI capability contract and register
through `gateway::extensions::registry::ExtensionApi`. The optional legacy regex
adapter is in application `gateway::extensions::guardrails`; it preserves global
input/output behavior and requires explicit installation.

### Run the official gateway or assemble an advanced host

Ordinary service consumers run `bro serve` with configuration and use its HTTP
interfaces. The SDK provides typed gateway, control and evaluation clients. Advanced trusted Rust
hosts can use `gateway::assembly::App` and the shared `host::serve_with_extensions`
lifecycle; adding compiled extension code requires rebuilding that custom host.

## Validation

Before opening a change, run the workspace checks from [`CONTRIBUTING.md`](../CONTRIBUTING.md):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI additionally runs `doc` (rustdoc under `-D warnings`), `doctest`, `feature-isolation` (the SDK's default tree stays free of axum, the OTel stack never reaches the SDK at any feature combination, and the two OTLP transports stay isolated), `sdk-public-api` (see below), and `msrv` (pinned to Rust 1.93). AI agents should also read [`CLAUDE.md`](../CLAUDE.md).

### Database backend regression

`database_backends` exercises migrations and gateway persistence through the
application's store APIs. Its ordinary SQLite case uses a temporary database.
The ignored server-backend case requires `BITROUTER_TEST_DATABASE_URL` pointing
to a dedicated test database; it applies migrations and writes uniquely scoped
fixture records. It never drops or truncates the database, and repeated runs
retain earlier fixture records. Do not point it at a service database.

```sh
cargo test -p bitrouter --no-default-features --test database_backends
BITROUTER_TEST_DATABASE_URL='postgres://fixture:fixture-password@127.0.0.1:5432/bitrouter_test' \
  cargo test -p bitrouter --no-default-features --features db-postgres \
  --test database_backends -- --include-ignored --test-threads=1
BITROUTER_TEST_DATABASE_URL='mysql://fixture:fixture-password@127.0.0.1:3306/bitrouter_test' \
  cargo test -p bitrouter --no-default-features --features db-mysql \
  --test database_backends -- --include-ignored --test-threads=1
```

CI provisions PostgreSQL 17 and MySQL 8.4 independently and runs each with its
own driver feature. This validates actual schema and store operations; a driver
compilation or SQL-string assertion is not evidence of backend execution.
Historical session formats and cross-profile compatibility have separate P0
acceptance gates.

The same target also checks owner-scoped shared record initialization and stale
revision rejection. With `core`, it checks that concurrent reliability appends
report one insertion and one duplicate, retain the original observation, and
reject conflicting reuse. `database_capture` requires `harness` and exercises
the actual recorder's new/fork/update path, first-writer metadata, owner/source
isolation and reconnect readback. Run both targets against the dedicated server:

```sh
BITROUTER_TEST_DATABASE_URL='mysql://fixture:fixture-password@127.0.0.1:3306/bitrouter_test' \
  cargo test -p bitrouter --no-default-features --features db-mysql,core,harness \
  --test database_backends --test database_capture -- --include-ignored --test-threads=1
```

Select `db-postgres` and a PostgreSQL test URL for the other backend. CI runs both
the gateway-only and core/harness selections; these checks do not promise
distributed execution or a complete historical session-format audit.

With `db-mysql`, `database_capture` also runs three ignored REPEATABLE READ races:
concurrent session creation, append after deletion, and native-ID reuse. A
connection-specific pending INSERT barrier fixes the interleaving; the test
checks persisted content as well as the returned result. Those cases require a
MySQL URL, so use the backend-specific profiles above rather than all features
when running against PostgreSQL.

The ignored library `mysql_tests` cases exercise shared-state revisions,
connection/coverage fences, checkpoint freezes and human assessment corrections.
They establish an old snapshot before a separate connection commits, then check
the mutation's current authority. The core profile also checks publication
against a concurrent human correction. Run them explicitly on the dedicated
MySQL test database:

```sh
BITROUTER_TEST_DATABASE_URL='mysql://fixture:fixture-password@127.0.0.1:3306/bitrouter_test' \
  cargo test -p bitrouter --no-default-features --features db-mysql,core,harness \
  --lib mysql_tests -- --ignored --test-threads=1
```

These tests retain MySQL's REPEATABLE READ behavior and the application's SQLite
write fences; they do not change the service's global isolation setting. Fixtures
use unique scopes, with only explicitly scoped cleanup/deletion. CI serializes
the cases so independent modules do not race initial migration setup.

### Public API dependency guards

Public API guards must track both AI contracts and the new SDK client surface.
AI must not expose service handles, credentials, database or OTel types, including
through trait bounds. The SDK must not expose the retired server framework.
The `sdk-public-api` job renders each package independently using the shared
renderer pins and compares its committed public-dependency manifest. Positive
contract/client sentinels precede checks for forbidden runtime types and public
forwarding exports. Normal/build dependency closures are checked separately.
Additional service DTO extraction must keep both checks passing.
Do not regenerate an API baseline merely to conceal an unwanted dependency.

### Named-router request checks

Named-router entry checks use AI capability types and the application
`gateway::pipeline::request_checks` runner. The app assembles startup-owned compiled registrations in
`request_checks`; a custom host supplies callbacks through
`extension::ExtensionApi`. The official `bro` binary has no custom
registrations, and there is no request-check inventory or receipt store.

Authentication and session/continuation normalization precede router binding.
The logical router/check binding remains fixed through candidate preparation,
model selection and fallback. One preparation path serves both response modes.
Checked routers apply effective defaults before content checks; ordinary local
hooks cannot change their selector afterward. Unguarded requests preserve the
legacy pre-request rewrite/defaults timing. The existing model selector and
executor run after required checks allow. Local policy rejection never invokes
a registered checker. The runtime emits bounded, content-free tracing for
registration and invocation outcomes; it does not expose checker inventory,
progress reporting or durable workflow state. Registration readiness and a
real invocation remain separate evidence.

See [REQUEST_CHECKS_SPEC.md](REQUEST_CHECKS_SPEC.md) for coverage, resource limits,
activation and the acceptance ledger. The default host no longer links
the matcher and rejects legacy `plugins.bitrouter-guardrails` configuration. See
[GUARDRAILS_EXTENSION.md](GUARDRAILS_EXTENSION.md) for custom-host assembly,
input-only scope, explicit migration blockers, and process-level verification.

### Native harness resources

`bitrouter-orchestrator::harness` owns direct MCP connections and local skills
discovery. The app supplies daemon-owned transport configuration and global roots;
active ThreadService Turns freeze inventories and execute MCP through existing
durable approval/tool barriers. Gateway interop and routed provider generation
retain their existing responsibilities. See [BRO harness resources](BRO_HARNESS_RESOURCES.md).
